use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;
use axum::{extract::{Path, State}, http::StatusCode, routing::{get, post}, Json, Router};
use serde::Serialize;
use uuid::Uuid;
use std::net::SocketAddr;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {

    let rooms: Rooms = Arc::new(RwLock::new(HashMap::new()));
    
    let app= Router::new()
    .route("/ok", get(|| async {"ok"}))
    .route("/rooms", post(create_room))
    .route("/rooms/{id}", get(get_room))
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

#[derive(Default)]
struct Room {

}

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