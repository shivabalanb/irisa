use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        ws::{CloseFrame, close_code},
    },
    http::StatusCode,
    response::Html,
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::net::SocketAddr;
use std::{collections::HashMap, sync::Arc};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tracing::{error, info, warn};
use tracing_subscriber;
use uuid::Uuid;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
enum ClientMessage {
    #[serde(rename = "offer")]
    Offer { sdp: String },
    #[serde(rename = "answer")]
    Answer { sdp: String },
    #[serde(rename = "ice")]
    Ice { candidate: Value },
    #[serde(rename = "bye")]
    Bye,
}

#[derive(Default)]
struct Room {
    peers: Vec<(String, PeerTx)>, // (peer_id, peer_tx)
}

type Rooms = Arc<RwLock<HashMap<String, Room>>>;

#[derive(Serialize, Deserialize)]
struct RoomId {
    roomId: String,
}

type PeerTx = mpsc::UnboundedSender<String>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();

    let rooms: Rooms = Arc::new(RwLock::new(HashMap::new()));

    let app = Router::new()
        .route("/", get(index)) // serve your page
        .route("/ok", get(|| async { "ok" }))
        .route("/ws", get(ws_handler))
        // .route("/rooms", post(create_room))
        // .route("/rooms/{id}", get(get_room))
        .with_state(rooms);

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let listener = TcpListener::bind(addr).await.expect("bind {addr} failed");
    println!("server on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(q): Query<RoomId>,
    State(rooms): State<Rooms>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, rooms, q.roomId))
}

async fn handle_socket(socket: WebSocket, rooms: Rooms, room_id: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<String>();
    let peer_id = Uuid::new_v4().to_string();

    let (role_opt, ready_targets): (Option<String>, Vec<PeerTx>) = {
        let mut guard = rooms.write().await;
        let room = guard.entry(room_id.clone()).or_default();

        // If already full, signal "reject" (handle after we release the lock)
        if room.peers.len() >= 2 {
            (None, Vec::new())
        } else {
            // Add this peer
            room.peers.push((peer_id.clone(), peer_tx.clone()));

            // Role is based on new length
            let role = if room.peers.len() == 1 {
                "host".to_string()
            } else {
                "guest".to_string()
            };

            // If this made the room reach 2 peers, notify both peers that the room is ready
            let ready_targets = if room.peers.len() == 2 {
                room.peers
                    .iter()
                    .map(|(_, peer_tx)| peer_tx.clone())
                    .collect()
            } else {
                Vec::new()
            };

            (Some(role), ready_targets)
        }
    };

    let role = match role_opt {
        Some(r) => r,
        None => {
            info!(%peer_id, %room_id, "room full → sending WS Close");
            let _ = ws_tx
                .send(Message::Close(Some(CloseFrame {
                    code: close_code::POLICY,
                    reason: "room full".into(),
                })))
                .await;
            return;
        }
    };

    let role_msg = serde_json::json!({ "type": "role", "role": role });
    if ws_tx
        .send(Message::Text(role_msg.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    info!(%peer_id, %room_id, %role, "peer joined");

    let ready_msg = serde_json::json!({ "type": "room", "status": "ready" });
    for peer_tx in ready_targets {
        let _ = peer_tx.send(ready_msg.to_string());
    }

    let rooms_fwd = rooms.clone();
    let room_id_fwd = room_id.clone();
    let peer_id_fwd = peer_id.clone();

    // Task A: Forward incoming messages from this peer to other peers in the room
    let mut forward_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Text(txt) => match serde_json::from_str::<ClientMessage>(&txt) {
                    Ok(ClientMessage::Bye) => {
                        info!(%peer_id_fwd, %room_id_fwd, "got BYE → forwarding and closing");

                        let targets: Vec<UnboundedSender<String>> = {
                            let rooms_map = rooms_fwd.read().await;
                            if let Some(room) = rooms_map.get(&room_id_fwd) {
                                room.peers
                                    .iter()
                                    .filter(|(pid, _)| pid != &peer_id_fwd)
                                    .map(|(_, tx)| tx.clone())
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        };
                        for other_peer_tx in targets {
                            let _ = other_peer_tx.send(txt.to_string());
                        }

                        break;
                    }
                    Ok(_valid) => {
                        let targets: Vec<UnboundedSender<String>> = {
                            let rooms_map = rooms_fwd.read().await;
                            if let Some(room) = rooms_map.get(&room_id_fwd) {
                                room.peers
                                    .iter()
                                    .filter(|(pid, _)| pid != &peer_id_fwd)
                                    .map(|(_, tx)| tx.clone())
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        };
                        for other_peer_tx in targets {
                            let _ = other_peer_tx.send(txt.to_string());
                        }
                    }
                    Err(e) => {
                        warn!(%peer_id_fwd, %room_id_fwd, error=%e, raw=%txt, "drop invalid message");
                    }
                },
                Message::Close(_f) => {
                    info!(%peer_id_fwd, %room_id_fwd, ?_f, "client sent WS Close");
                    break;
                }
                _ => {
                    info!(%peer_id_fwd, %room_id_fwd, "ignoring non-text");
                }
            }
        }
    });

    // Task B: Send messages from other peers to this peer's WebSocket
    let mut pump_task = tokio::spawn(async move {
        while let Some(txt) = peer_rx.recv().await {
            if ws_tx.send(Message::Text(txt.into())).await.is_err() {
                break;
            }
        }
    });

    tokio::select! {
        _ = &mut forward_task => {
            info!(%peer_id, %room_id, "forward ended → abort pump");
            pump_task.abort();
        }
        _ = &mut pump_task => {
            info!(%peer_id, %room_id, "pump ended → abort forward");
            forward_task.abort();
        }
    }

    {
        let mut guard = rooms.write().await;
        if let Some(room) = guard.get_mut(&room_id) {
            // Delete the room if no peers remain
            if room.peers.is_empty() {
                guard.remove(&room_id);
            } else {
                // Remove this peer from the room
                room.peers
                    .retain(|(pid, tx)| pid != &peer_id && !tx.is_closed());
            }
        }
    }
}

// async fn create_room(rooms: State<Rooms>) -> (StatusCode, Json<RoomId>) {
//     let id = Uuid::new_v4().to_string();
//     {
//         let mut table = rooms.write().await;
//         table.insert(id.clone(), Room::default());
//     }
//     let body = RoomId { roomId: id };
//     (StatusCode::CREATED, Json(body))
// }

// #[derive(Serialize)]
// struct RoomInfo {
//     exists: bool,
// }

// async fn get_room(
//     State(rooms): State<Rooms>,
//     Path(id): Path<String>,
// ) -> (StatusCode, Json<RoomInfo>) {
//     let table = rooms.read().await;
//     let exists = table.contains_key(&id);
//     let code = if exists {
//         StatusCode::OK
//     } else {
//         StatusCode::NOT_FOUND
//     };
//     return (code, Json(RoomInfo { exists }));
// }
