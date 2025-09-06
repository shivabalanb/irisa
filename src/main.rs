use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use axum::{extract::{ws::{close_code, CloseFrame}, Path, Query, State}, http::StatusCode, routing::{get, post}, Json, Router};
use serde::Serialize;
use uuid::Uuid;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{StreamExt, SinkExt};
use serde::Deserialize;
use tokio::sync::mpsc;

type PeerTx = mpsc::UnboundedSender<String>;

#[derive(Default)]
struct Room {
    peers: Vec<(String, PeerTx)>, // (peed_id, tx_to_that_peer)
}

#[derive(Deserialize)]
struct WsParams {
    roomId: String,
}


#[tokio::main]
async fn main() {

    let rooms: Rooms = Arc::new(RwLock::new(HashMap::new()));
    
    let app= Router::new()
    .route("/ok", get(|| async {"ok"}))
    .route("/rooms", post(create_room))
    .route("/rooms/{id}", get(get_room))
    .route("/ws", get(ws_handler))
    .with_state(rooms);

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let listener = TcpListener::bind(addr).await.expect("bind {addr} failed");
    println!("server on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}  

#[derive(Serialize)]
struct RoomRes {
    roomId: String,  
}

async fn create_room(rooms: State<Rooms>) -> (StatusCode, Json<RoomRes>){
    let id = Uuid::new_v4().to_string();
    {
        let mut table = rooms.write().await;
        table.insert(id.clone(), Room::default());
    }
    let body = RoomRes { roomId: id };
    (StatusCode::CREATED, Json(body))
}

type Rooms = Arc<RwLock<HashMap<String, Room>>>;

#[derive(Serialize)]
struct RoomInfo {
    exists: bool,
}

async fn get_room(State(rooms): State<Rooms>, Path(id): Path<String>,) -> (StatusCode, Json<RoomInfo>){
    let table = rooms.read().await;
    let exists = table.contains_key(&id);
    let code = if exists { StatusCode::OK} else { StatusCode::NOT_FOUND};
    return (code, Json(RoomInfo { exists }));
}

async fn ws_handler(ws: WebSocketUpgrade, Query(q): Query<WsParams>, State(rooms): State<Rooms>) -> impl IntoResponse{
    ws.on_upgrade(move |socket|handle_socket(socket, rooms, q.roomId))
}

async fn handle_socket(socket: WebSocket, rooms: Rooms, room_id: String) {
    let (mut sender, mut receiver) = socket.split();
    let (tx_to_me, mut rx_to_me) = mpsc::unbounded_channel::<String>();
    let peer_id = Uuid::new_v4().to_string();

    // Step 1: Register this peer in the room and determine their role
    let role = {
        let mut guard = rooms.write().await;
        let room = guard.entry(room_id.clone()).or_default();

        // Check if room is full (max 2 peers)
        if room.peers.len() >= 2 {
            let _ = sender.send(Message::Close(Some(CloseFrame {
                code: close_code::POLICY, 
                reason: "room full".into()
            }))).await;
            return;
        }

        // Add this peer to the room
        room.peers.push((peer_id.clone(), tx_to_me.clone()));
        
        // Determine role: first peer is caller, second is callee
        if room.peers.len() == 1 { 
            "caller".to_string()
        } else { 
            "callee".to_string()
        }
    };

    // Send role assignment to the peer
    let _ = sender.send(Message::Text(format!(
        r#"{{"type":"role","role":"{}"}}"#, role
    ).into())).await;

    // Clone rooms for the forwarding task (to avoid borrow checker issues)
    let rooms_for_forwarding = rooms.clone();
    let room_id_for_forwarding = room_id.clone();
    let peer_id_for_forwarding = peer_id.clone();

    // Task A: Forward incoming messages from this peer to other peers in the room
    let forward_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(txt) = msg {
                let mut guard = rooms_for_forwarding.write().await;
                if let Some(room) = guard.get_mut(&room_id_for_forwarding) {
                    // Forward message to all other peers in the room
                    for (other_peer_id, peer_tx) in &room.peers {
                        if other_peer_id != &peer_id_for_forwarding {
                            let _ = peer_tx.send(txt.to_string());
                        }
                    }
                }
            }
        }
    });

    // Task B: Send messages from other peers to this peer's WebSocket
    let pump_task = tokio::spawn(async move {
        while let Some(txt) = rx_to_me.recv().await {
            if sender.send(Message::Text(txt.into())).await.is_err() {
                break;
            }
        }
    });

    // Wait for both tasks to complete (when WebSocket connection closes)
    let _ = tokio::join!(forward_task, pump_task);

    // Cleanup: Remove this peer from the room and delete room if empty
    let mut guard = rooms.write().await;
    if let Some(room) = guard.get_mut(&room_id) {
        // Remove this peer from the room
        room.peers.retain(|(pid, tx)| {
            pid != &peer_id && !tx.is_closed()
        });
        
        // Delete the room if no peers remain
        if room.peers.is_empty() {
            guard.remove(&room_id);
        }
    }
}
