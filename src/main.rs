use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::{
    Router,
    extract::{
        Query, State,
        ws::{CloseFrame, close_code},
    },
    response::Html,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::net::SocketAddr;
use std::{collections::HashMap, sync::Arc};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::sync::mpsc::{self, UnboundedSender};
use tracing::{info, warn};
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

type PeerTx = mpsc::UnboundedSender<String>;

#[derive(Serialize, Deserialize, Clone, Debug)]
enum Role {
    Host,
    Guest,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Host => write!(f, "host"),
            Role::Guest => write!(f, "guest"),
        }
    }
}

#[derive(Default)]
struct Room {
    peers: Vec<(Role, String, PeerTx)>, // (role,peer_id, peer_tx)
}

type RoomRegistry = Arc<RwLock<HashMap<String, Room>>>;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();

    let room_registry: RoomRegistry = Arc::new(RwLock::new(HashMap::new()));

    let app = Router::new()
        .route("/", get(index)) // serve your page
        .route("/ok", get(|| async { "ok" }))
        .route("/ws", get(ws_handler))
        // .route("/rooms", post(create_room))
        // .route("/rooms/{id}", get(get_room))
        .with_state(room_registry);

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
    Query(params): Query<HashMap<String, String>>,
    State(room_registry): State<RoomRegistry>,
) -> impl IntoResponse {
    let room_id = params.get("roomId").unwrap().clone();
    ws.on_upgrade(move |socket| handle_socket(socket, room_registry, room_id))
}

async fn handle_socket(socket: WebSocket, room_registry: RoomRegistry, room_id: String) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<String>();
    let peer_id = Uuid::new_v4().to_string();

    let (role, peer_channels): (Role, Vec<PeerTx>) = {
        let mut registry_guard = room_registry.write().await;
        let room = registry_guard.entry(room_id.clone()).or_default();

        let role = if room.peers.is_empty() {
            Role::Host
        } else {
            Role::Guest
        };

        room.peers
            .push((role.clone(), peer_id.clone(), peer_tx.clone()));

        let peer_channels = room
            .peers
            .iter()
            .map(|(_, _, peer_tx)| peer_tx.clone())
            .collect();

        (role, peer_channels)
    };

    // Notify new peer about their role
    let role_msg = serde_json::json!({ "type": "role", "role": role.to_string(), "peerId": peer_id });
    if ws_tx
        .send(Message::Text(role_msg.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    info!(%peer_id, %room_id, %role, "peer joined");

    // Notify new peer about the number of peers in the room
    let peer_joined_msg = serde_json::json!({ 
        "type": "room", 
        "event": "peer-joined", 
        "peerCount": peer_channels.len(),
        "peerId": peer_id 
    });
    for peer_tx in peer_channels {
        let _ = peer_tx.send(peer_joined_msg.to_string());
    }

    let room_registry_fwd = room_registry.clone();
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
                            let rooms_map = room_registry_fwd.read().await;
                            if let Some(room) = rooms_map.get(&room_id_fwd) {
                                room.peers
                                    .iter()
                                    .filter(|(_, pid, _)| pid != &peer_id_fwd)
                                    .map(|(_, _, tx)| tx.clone())
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
                            let rooms_map = room_registry_fwd.read().await;
                            if let Some(room) = rooms_map.get(&room_id_fwd) {
                                room.peers
                                    .iter()
                                    .filter(|(_, pid, _)| pid != &peer_id_fwd)
                                    .map(|(_, _, tx)| tx.clone())
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
        let mut guard = room_registry.write().await;
        if let Some(room) = guard.get_mut(&room_id) {
            // Remove this peer from the room
            room.peers
                .retain(|(_, pid, tx)| pid != &peer_id && !tx.is_closed());

            // If there's still a peer remaining, notify them that the other peer left
            if !room.peers.is_empty() {
                // The remaining peer becomes the host
                let peer_left_msg = serde_json::json!({ "type": "peer-left", "newRole": "host", "peerId": peer_id });
                for (_, _, peer_tx) in &room.peers {
                    let _ = peer_tx.send(peer_left_msg.to_string());
                }
                info!(%peer_id, %room_id, "notified remaining peer that peer left, they are now host");
            } else {
                // Delete the room if no peers remain
                guard.remove(&room_id);
                info!(%peer_id, %room_id, "room deleted - no peers remaining");
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
