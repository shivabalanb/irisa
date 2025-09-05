use axum::{http::StatusCode, routing::{get, post}, Json, Router};
use serde::Serialize;
use uuid::Uuid;
use std::net::SocketAddr;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    let app= Router::new()
    .route("/ok", get(|| async {"ok"}))
    .route("/rooms", post(create_room));

    let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
    let listener = TcpListener::bind(addr).await.expect("bind {addr} failed");
    println!("server on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}  

#[derive(Serialize)]
struct RoomRes {
    roomId: String,  
}

async fn create_room() -> (StatusCode, Json<RoomRes>){
    let id = Uuid::new_v4().to_string();
    let body = RoomRes { roomId: id };
    (StatusCode::CREATED, Json(body))
}

