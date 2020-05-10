use serde::{Serialize,Deserialize};
use serde::de::DeserializeOwned;
use serde_json;
use actix_web::{HttpServer,App,web,Responder,HttpRequest,HttpResponse};
use actix_web::middleware::Logger;
use futures::prelude::*;
use redis::{RedisResult,AsyncCommands,RedisError};
use redis::aio::MultiplexedConnection;
#[macro_use] extern crate log;
use env_logger::Env;
use uuid::Uuid;
use chrono::{DateTime,Utc};

use std::sync::Arc;
use std::sync::Mutex;

/// HTTP app state.
struct AppState {
    redis_conn: Mutex<MultiplexedConnection>,
}

/// Provides the key in Redis which an object will be stored.
trait RedisKey {
    /// Redis key.
    fn key(&self) -> String;
}

/// Store item as JSON in Redis
async fn store_in_redis<T: Serialize + RedisKey>(
    redis_conn: &mut MultiplexedConnection,
    data: &T
) -> Result<(), String>
{
    let data_json = match serde_json::to_string(&data) {
        Err(e) => return Err(format!("Failed to serialize data: {}", e)),
        Ok(v) => v,
    };

    let _resp: String = match redis_conn.set(data.key(), data_json).await {
        Err(e) => return Err(format!("Failed to set data in Redis: {}", e)),
        Ok(v) => v,
    };

    Ok(())
}

/// Retreive an item represented in Redis as JSON
async fn load_from_redis<T: DeserializeOwned + RedisKey>(
    redis_conn: &mut MultiplexedConnection,
    key: &T
) -> Result<Option<T>, String>
{
    let data_json = match redis_conn.get::<String, Option<String>>(key.key()).await
    {
        Err(e) => return Err(format!("Failed to get data from Redis: {}", e)),
        Ok(v) => match v {
            None => return Ok(None),
            Some(v) => v,
        },
    };

    match serde_json::from_str::<T>(data_json.as_str()) {
        Err(e) => Err(format!("Failed to deserialize data: {}", e)),
        Ok(v) => Ok(Some(v)),
    }
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

    /// Last time the session was updated. Used to find old sessions and
    /// delete them. Value is the number of non-leap seconds since EPOCH in UTC.
    last_updated: i64,
}

impl RedisKey for SyncSession {
    fn key(&self) -> String {
        format!("sync_session:{}", self.id)
    }
}

impl SyncSession {
    /// Creates a new sync session with only the id field populated. All other
    /// fields have default values. These should be replaced before use.
    fn new_only_id(id: String) -> SyncSession {
        SyncSession{
            id: id,
            name: String::from(""),
            playing: false,
            timestamp_seconds: 0,
            last_updated: 0,
        }
    }
}

/// Response which all non-200 responses will use.
#[derive(Serialize)]
struct ErrorResp<'a> {
    /// User error message.
    error: &'a str,
}

impl <'a> ErrorResp<'a> {
    fn new(user_error: &'a str, internal_error: &'a str) -> ErrorResp<'a> {
        error!("user error={}, internal error={}", user_error, internal_error);
        ErrorResp{
            error: user_error,
        }
    }
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

/// Request for create sync session endpoint.
#[derive(Deserialize)]
struct CreateSyncSessionReq {
    /// Name of sync session to create.
    name: String,
}

/// Response of create and get sync session endpoint.
#[derive(Serialize)]
struct ASyncSessionResp {
    /// Creted sync session.
    sync_session: SyncSession,
}

/// Creates a new sync session.
async fn create_sync_session(
    data: web::Data<AppState>,
    req: web::Json<CreateSyncSessionReq>
) -> impl Responder
{
    let sess = SyncSession{
        id: Uuid::new_v4().to_string(),
        name: req.name.clone(),
        playing: false,
        timestamp_seconds: 0,
        last_updated: Utc::now().timestamp(),
    };

    match store_in_redis(&mut data.redis_conn.lock().unwrap(), &sess).await {
        Err(e) => return HttpResponse::InternalServerError().json(
            ErrorResp::new("Failed to save new sync session", &e)),
        _ => (),
    };
    
    HttpResponse::Ok().json(ASyncSessionResp{
        sync_session: sess,
    })
}

/// Retrieves a sync session by ID.
async fn get_sync_session(
    data: web::Data<AppState>,
    urldata: web::Path<String>
) -> impl Responder
{
    let sess_key = SyncSession::new_only_id(urldata.into_inner());
    
    let sess = match load_from_redis(&mut data.redis_conn.lock().unwrap(),
                                     &sess_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ErrorResp::new("Failed to get sync session", &e)),
        Ok(v) => v,
    };

    match sess {
        None => HttpResponse::NotFound().json(
            ErrorResp::new("Not found", "No item with key in redis")),
        Some(v) => HttpResponse::Ok().json(ASyncSessionResp{
            sync_session: v,
        }),
    }
}

/// Default handler when no registered routes match a request.
async fn not_found(req: HttpRequest) -> impl Responder
{
    HttpResponse::NotFound().json(
        ErrorResp::new("Not found", &format!("{} does not exist", req.path())))
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    // Setup logger
    env_logger::from_env(Env::default().default_filter_or("debug")).init();
    
    // Connect to Redis
    info!("Connecting to Redis");
    let redis_client = match redis::Client::open("redis://127.0.0.1/") {
        Err(e) => return Err(
            std::io::Error::new(std::io::ErrorKind::Other,
                                format!("Failed to open redis connection: {}",
                                        e))),
        Ok(v) => v,
    };
    let (mut redis_conn, redis_driver) = match
        redis_client.get_multiplexed_async_connection().await
    {
        Err(e) => return Err(
            std::io::Error::new(std::io::ErrorKind::Other,
                                format!("Failed to get redis connection: {}",
                                        e))),
        Ok(v) => v,
        
    };

    actix_rt::spawn(redis_driver);

    info!("Testing Redis connection");
    
    let ping_res: RedisResult<String> = redis::cmd("PING")
        .query_async(&mut redis_conn).await;
    if ping_res != Ok("PONG".to_string()) {
        return Err(
            std::io::Error::new(std::io::ErrorKind::Other,
                                format!("Redis ping connection test failed: {:?}",
                                        ping_res)));
    }

    info!("Successfully connected to Redis");

    // Start HTTP server
    let app_state = web::Data::new(AppState{
        redis_conn: Mutex::new(redis_conn),
    });
    
    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .route("/api/v0/status", web::get().to(server_status))
            .route("/api/v0/sync_session", web::post().to(create_sync_session))
            .route("/api/v0/sync_session/{id}", web::get().to(get_sync_session))
            .default_service(web::route().to(not_found))
            .wrap(Logger::default())
    })
        .bind("0.0.0.0:8000")?
        .run()
        .await

   
}
