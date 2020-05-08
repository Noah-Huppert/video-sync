use serde::{Serialize,Deserialize};
use actix_web::{HttpServer,App,web,Responder,HttpResponse};
use actix_web::middleware::Logger;
use futures::prelude::*;
use redis::{RedisResult,AsyncCommands,RedisError};
#[macro_use] extern crate log;
use env_logger::Env;

use std::sync::Arc;

/// HTTP app state.
struct AppState {
    redis_conn: redis::aio::Connection,
}

/// A video sync session which holds video state.
#[derive(Serialize,Deserialize)]
struct SyncSession {
    /// Unique identifier.
    id: String,

    /// Name of session.
    name: String,

    /// If video is running.
    playing: bool,

    /// Number of seconds into video.
    timestamp_seconds: i32,
}

/// Response from status endpoint.
#[derive(Serialize)]
struct StatusResp {
    /// If server is functioning.
    ok: bool,
}

/// Returns the server's status.
async fn server_status() -> impl Responder {
    HttpResponse::Ok().json(StatusResp{
        ok: true,
    })
}

#[derive(Serialize)]
struct CreateSyncSessionResp {
    sync_session: SyncSession,
}

/// Creates a new sync session.
async fn create_sync_session(data: web::Data<AppState>) -> impl Responder {
    HttpResponse::Ok().json(StatusResp{
        ok: true,
    })
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    // Setup logger
    env_logger::from_env(Env::default().default_filter_or("info")).init();
    
    // Connect to Redis
    let redis_client = match redis::Client::open("redis://127.0.0.1/") {
        Err(e) => return Err(
            std::io::Error::new(std::io::ErrorKind::Other,
                                format!("Failed to open redis connection: {}", e))),
        Ok(v) => v,
    };
    let mut redis_conn = match redis_client.get_async_connection().await {
        Err(e) => return Err(
            std::io::Error::new(std::io::ErrorKind::Other,
                                format!("Failed to get redis connection: {}", e))),
        Ok(v) => v,
                                                 
    };

    let ping_res: RedisResult<String> = redis::cmd("PING")
        .query_async(&mut redis_conn).await;
    if ping_res != Ok("PONG".to_string()) {
        return Err(
            std::io::Error::new(std::io::ErrorKind::Other,
                                format!("Redis ping connection test failed: {:?}",
                                        ping_res)));
    }

    // Start HTTP server
    let app_state = web::Data::new(AppState{
        redis_conn: redis_conn,
    });
    
    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .route("/api/v0/status", web::get().to(server_status))
            .route("/api/v0/sync", web::post().to(create_sync_session))
            .wrap(Logger::default())
    })
        .bind("0.0.0.0:8000")?
        .run()
        .await

   
}
