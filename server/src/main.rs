#[macro_use] extern crate serde;
use serde_json;
use serde::{Serialize,Deserialize};
use serde::de::DeserializeOwned;
use actix_web::{HttpServer,App,web,Responder,HttpRequest,HttpResponse};
use actix_web::middleware::Logger;
use redis::{RedisResult,AsyncCommands};
use redis::aio::MultiplexedConnection;
#[macro_use] extern crate log;
use env_logger::Env;
use uuid::Uuid;
use chrono::Utc;

use std::collections::HashMap;
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

/// Store item as JSON in Redis.
async fn store_in_redis<T: Serialize + RedisKey>(
    redis_conn: &mut MultiplexedConnection,
    data: &T
) -> Result<(), String>
{
    let data_json = match serde_json::to_string(data) {
        Err(e) => return Err(format!("Failed to serialize data as JSON: {}", e)),
        Ok(v) => v,
    };

    let _resp: String = match redis_conn.set(data.key(), data_json).await {
        Err(e) => return Err(format!("Failed to set data in Redis: {}", e)),
        Ok(v) => v,
    };

    Ok(())
}

/// Holds context required to setup multiple set commands on a Redis pipeline.
struct RedisStoreMany<'a> {
    /// Pipeline used to execute the multi set operation.
    redis_pipeline: &'a mut redis::Pipeline,

    /// Data to store. Keys = Redis key, values = JSON
    data: HashMap<String, String>,

    /// Errors which occured during serialization in the store() method.
    /// Keys = Redis key, values = error strings.
    serialization_errors: HashMap<String, String>,
}

impl <'a> RedisStoreMany<'a> {
    /// Creates a new RedisStoreMany structure.
    fn new(redis_pipeline: &'a mut redis::Pipeline) -> RedisStoreMany {
        RedisStoreMany{
            redis_pipeline: redis_pipeline,
            data: HashMap::new(),
            serialization_errors: HashMap::new(),
        }
    }

    /// Add item to be stored. Determines the key and serializes item to JSON.
    /// If an error occurs during serialization it is not returned until execute()
    /// so that this method can be chained and called multiple times without error
    /// checking each time.
    fn store<T: Serialize + RedisKey>(&mut self, data: &T)
                                      -> &'a mut RedisStoreMany
    {
        let key = data.key();

        let json = match serde_json::to_string(data) {
            Err(e) => {
                self.serialization_errors.insert(key, e.to_string());
                return self;
            },
            Ok(v) => v,
        };

        self.data.insert(key, json);
        
        self
    }

    /// Serialize items and store in Redis.
    async fn execute(&mut self, redis_conn: &mut MultiplexedConnection)
               -> Result<(), String>
    {
        if self.serialization_errors.len() > 0 {
            let mut error_str = String::from("Serialization errors occurred: ");

            let mut i = 0;
            for (key, e) in &self.serialization_errors {
                if i > 0 {
                    error_str.push_str(", ");
                }
                
                error_str.push_str(&format!("Failed to serialize item at key {}\
                                             as JSON: {}", key, e));
                i += 1;
            }

            return Err(error_str);
        }

        for (key, data_json) in &self.data {
            self.redis_pipeline.cmd("SET").arg(key).arg(data_json);
        }

        match self.redis_pipeline.query_async::<MultiplexedConnection, Vec<String>>(
            redis_conn).await
        {
            Err(e) => return Err(format!("Failed to execute set pipeline: {}", e)),
            _ => Ok(()),
        }
    }
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
/// Times are the number of non-leap seconds since EPOCH in UTC.
#[derive(Serialize,Deserialize)]
struct SyncSession {
    /// Identifier, UUIDv4.
    id: String,

    /// Name of session.
    name: String,

    /// If video is running.
    playing: bool,

    /// Number of seconds into video when it was the time specified
    /// by last_updated.
    timestamp_seconds: i32,

    /// Client's time when they updated timestamp_seconds.
    timestamp_last_updated: i64,

    /// Last time any information about session was updated, including
    /// timestamp_seconds This time is the server's time. Since it is only used
    /// to find old sessions. User updates are not included.
    last_updated: i64,
}

impl RedisKey for SyncSession {
    fn key(&self) -> String {
        format!("sync_session:{}", self.id)
    }
}

impl SyncSession {
    /// Creates a new sync session with only the id field populated. This allows
    /// the structure to function properly as a RedisKey. All nother fields have
    /// empty values which should be replaced before use. 
    fn new_for_key(id: String) -> SyncSession {
        SyncSession{
            id: id,
            name: String::from(""),
            playing: false,
            timestamp_seconds: 0,
            timestamp_last_updated: 0,
            last_updated: 0,
        }
    }
}

/// User in a sync session.
/// Times are the number of non-leap seconds since EPOCH in UTC.
#[derive(Serialize)]
struct User {
    /// Identifier UUIDv4. This value is treated as a secret which only the
    /// user themselves know.
    #[serde(skip)]
    id: String,

    /// Identifier of sync session user belongs to.
    sync_session_id: String,
    
    /// Friendly name to identify user.
    name: String,

    /// Last time the client was seen from the server's perspective.
    last_seen: i64,
}

impl RedisKey for User {
    fn key(&self) -> String {
        format!("sync_session:{}:user:{}", self.sync_session_id, self.id)
    }
}

impl User {
    /// Creates a user with the id and sync_session_id fields populated. This
    /// allows the structure to function properly as a RedisKey. All other fields
    /// are given empty values and should be replaced before use.
    fn new_for_key(sync_session_id: String, id: String) -> User {
        User{
            id: id,
            sync_session_id: sync_session_id,
            name: String::from(""),
            last_seen: 0,
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

/// Response of create sync session endpoint.
#[derive(Serialize)]
struct CreateSyncSessionResp {
    /// Created sync session.
    sync_session: SyncSession,

    /// User to be used by client who just created the sync session.
    user: User,

    /// Secret ID of user to be used by client who just crated the sync session.
    user_id: String,
}

/// Creates a new sync session.
async fn create_sync_session(
    data: web::Data<AppState>,
    req: web::Json<CreateSyncSessionReq>
) -> impl Responder
{
    let now = Utc::now().timestamp();
    
    let sess = SyncSession{
        id: Uuid::new_v4().to_string(),
        name: req.name.clone(),
        playing: false,
        timestamp_seconds: 0,
        timestamp_last_updated: now,
        last_updated: now,
    };

    let user = User{
        id: Uuid::new_v4().to_string(),
        sync_session_id: String::from(&sess.id),
        name: String::from("Admin"),
        last_seen: now,
    };

    let mut redis_pipeline = redis::pipe();
    redis_pipeline.atomic();

    match RedisStoreMany::new(&mut redis_pipeline)
        .store(&sess)
        .store(&user)
        .execute(&mut data.redis_conn.lock().unwrap()).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ErrorResp::new("Failed to save new sync session", &e)),
        _ => (),
    };
    
    HttpResponse::Ok().json(CreateSyncSessionResp{
        sync_session: sess,
        user_id: String::from(&user.id),
        user: user,
    })
}

/// Get sync session endpoint response.
#[derive(Serialize)]
struct GetSyncSessionResp {
    /// Retrieved sync session.
    sync_session: SyncSession,
}

/// Retrieves a sync session by ID.
async fn get_sync_session(
    data: web::Data<AppState>,
    urldata: web::Path<String>
) -> impl Responder
{
    let sess_key = SyncSession::new_for_key(urldata.into_inner());
    
    let sess = match load_from_redis(&mut data.redis_conn.lock().unwrap(),
                                     &sess_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ErrorResp::new("Failed to get sync session", &e)),
        Ok(v) => v,
    };

    match sess {
        None => HttpResponse::NotFound().json(
            ErrorResp::new(&format!("Sync session {} not found", sess_key.id),
                           "Not specified")),
        Some(v) => HttpResponse::Ok().json(GetSyncSessionResp{
            sync_session: v,
        }),
    }
}

/// Update sync session metadata request.
#[derive(Deserialize)]
struct UpdateSyncSessMetaReq {
    /// New name for sync session, None if the name should not be updated.
    name: Option<String>,
}

/// Update sync session metadata response.
#[derive(Serialize)]
struct UpdateSyncSessMetaResp {
    /// Updated sync session.
    sync_session: SyncSession,
}

/// Updates a sync session's metadata.
async fn update_sync_session_metadata(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: web::Json<UpdateSyncSessMetaReq>,
) -> impl Responder
{
    let sess_key = SyncSession::new_for_key(urldata.into_inner());

    let mut sess = match load_from_redis(&mut data.redis_conn.lock().unwrap(),
                                         &sess_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ErrorResp::new("Error finding sync session to update",
                           &format!("Failed to get sync session: {}", e))),
        Ok(v) => match v {
            None => return HttpResponse::NotFound().json(
                ErrorResp::new(&format!("Sync session {} not found", sess_key.id),
                               "Not specified")),
            Some(v) => v,
        },
    };

    if let Some(new_name) = &req.name {
        sess.name = String::from(new_name);
    } else {
        return HttpResponse::BadRequest().json(
            ErrorResp::new("Request must contain at least one field to update",
                           "Not specified"));
    }

    sess.last_updated = Utc::now().timestamp();

    match store_in_redis(&mut data.redis_conn.lock().unwrap(), &sess).await {
        Err(e) => return HttpResponse::InternalServerError().json(
            ErrorResp::new("Failed to update sync session", &e)),
        _ => (),
    }

    HttpResponse::Ok().json(UpdateSyncSessMetaResp{
        sync_session: sess,
    })
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
            .route("/api/v0/sync_session/{id}", web::put()
                   .to(update_sync_session_metadata))
            .default_service(web::route().to(not_found))
            .wrap(Logger::default())
    })
        .bind("0.0.0.0:8000")?
        .run()
        .await

   
}
