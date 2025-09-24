use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::{
    Router,
    extract::{
        Query, State,
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
}

type PeerTx = mpsc::UnboundedSender<String>;

#[derive(Default)]
struct Room {
    peers: Vec<(String, PeerTx)>, // (peer_id, peer_tx)
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

    // Add new peer to the room
    {
        let mut registry_guard = room_registry.write().await;
        let room = registry_guard.entry(room_id.clone()).or_default();
        room.peers.push((peer_id.clone(), peer_tx.clone()));
    }
    
    let peer_channels: Vec<PeerTx> = {
        let registry_guard = room_registry.read().await;
        if let Some(room) = registry_guard.get(&room_id) {
            room.peers.iter().map(|(_, peer_tx)| peer_tx.clone()).collect()
        } else {
            Vec::new()
        }
    };

    let peer_joined_msg = serde_json::json!({ 
        "type": "peer-joined", 
        "peerId": peer_id 
    });
    
    // Notify all peers in the room 
    for peer_tx in peer_channels {
        let _ = peer_tx.send(peer_joined_msg.to_string());
    }

    info!(%peer_id, %room_id, "peer joined");

    let room_registry_fwd = room_registry.clone();
    let room_id_fwd = room_id.clone();
    let peer_id_fwd = peer_id.clone();

    // Task A: Forward incoming messages from this peer to other peers in the room
    let mut forward_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Text(txt) => match serde_json::from_str::<ClientMessage>(&txt) {
                    Ok(_valid) => {
                        // Parse the message to check for targetPeer
                        if let Ok(msg_json) = serde_json::from_str::<serde_json::Value>(&txt) {
                            let target_peer = msg_json.get("targetPeer").and_then(|v| v.as_str());
                            
                            let targets: Vec<UnboundedSender<String>> = {
                                let rooms_map = room_registry_fwd.read().await;
                                if let Some(room) = rooms_map.get(&room_id_fwd) {
                                    if let Some(target_peer_id) = target_peer {
                                        // Send to specific target peer
                                        room.peers
                                            .iter()
                                            .filter(|(pid, _)| pid == target_peer_id)
                                            .map(|(_, tx)| tx.clone())
                                            .collect()
                                    } else {
                                        // No targetPeer specified, broadcast to all others
                                        room.peers
                                            .iter()
                                            .filter(|(pid, _)| pid != &peer_id_fwd)
                                            .map(|(_, tx)| tx.clone())
                                            .collect()
                                    }
                                } else {
                                    Vec::new()
                                }
                            };
                            for other_peer_tx in targets {
                                let _ = other_peer_tx.send(txt.to_string());
                            }
                        }
                    }
                    Err(e) => {
                        warn!(%peer_id_fwd, %room_id_fwd, error=%e, raw=%txt, "drop invalid message");
                    }
                },
                Message::Close(_f) => {
                    info!(%peer_id_fwd, %room_id_fwd, ?_f, "client closed connection");
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
                .retain(|(pid, tx)| pid != &peer_id && !tx.is_closed());

            // If there's no peer remaining, delete the room
            if room.peers.is_empty() {
                guard.remove(&room_id);
                info!(%peer_id, %room_id, "room deleted - no peers remaining");
            }
        }
    }
}
