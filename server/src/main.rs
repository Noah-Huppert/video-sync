use serde_json;
use serde::{Serialize,Deserialize};
use serde::de::DeserializeOwned;

use actix_web::{web,http,HttpServer,App,Responder,HttpRequest,HttpResponse};
use actix_web::dev::{ServiceResponse,ResponseBody};
use actix_web::middleware::Logger;
use actix_web::middleware::errhandlers::{ErrorHandlerResponse,ErrorHandlers};

use redis::{RedisResult,AsyncCommands,FromRedisValue};
use redis::aio::MultiplexedConnection;

#[macro_use] extern crate log;
use env_logger::Env;
use uuid::Uuid;
use chrono::Utc;

use std::collections::HashMap;
use std::sync::Mutex;

/// Number of seconds sync session and user keys last in Redis. Set to 1 day.
const REDIS_KEY_TTL: usize = 86400;

/// HTTP app state.
struct AppState {
    /// Asynchronous Redis connection. Preferable to use when possible.
    redis_conn: Mutex<MultiplexedConnection>,
}

/// Provides the key in Redis under which an object will be stored.
trait RedisKey {
    /// Redis key for the specific item. The returned result can be used with the
    /// Redis SCAN command if the object's key fields are set to "*".
    fn key(&self) -> String;
}

/// Implements the RedisKey trait by returning a static String value.
struct StringRedisKey {
    key: String,
}

impl StringRedisKey {
    /// Creates a new StringRedisKey.
    fn new(key: String) -> StringRedisKey {
        StringRedisKey{
            key: key,
        }
    }
}

impl RedisKey for StringRedisKey {
    fn key(&self) -> String {
        String::from(self.key.as_str())
    }
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

    /// Time to live values for any keys in data. Keys = Redis key,
    /// values = time to live in seconds.
    ttls: HashMap<String, usize>,

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
            ttls: HashMap::new(),
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

    /// Sets a key's expiration time to live in seconds. Included in this struct
    /// because setting a key clears any existing TTL. The store() method must be
    /// called with the same key for this method to have any effect.
    fn expire<T: RedisKey>(&mut self, key: &T, ttl: usize)
                           -> &'a mut RedisStoreMany
    {
        self.ttls.insert(key.key(), ttl);

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

            match self.ttls.get(key) {
                Some(ttl) => {
                    self.redis_pipeline.cmd("EXPIRE").arg(key).arg(*ttl);
                    ()
                },
                None => (),
            };
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
async fn load_from_redis<T: DeserializeOwned, K: RedisKey>(
    redis_conn: &mut MultiplexedConnection,
    key: &K
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

/// Implements Stream like behavior by calling the Redis SCAN command and using the
/// returned cursor to fetch more items when required. Returns a Result<T, String>
/// because an error could occur when fetching the newest bunch of values from
/// Redis. The actual Stream trait was not implemented because I got caught up in
/// Rust semantics.
struct RedisScanStream<'a, T: FromRedisValue> {
    /// Redis connection used to execute SCAN commands.
    redis_conn: &'a mut MultiplexedConnection,

    /// SCAN command MATCH <pattern> argument. None if a match argument should
    /// not be sent.
    match_arg: Option<String>,

    /// Items retrieved by last SCAN command invocation.
    items: Vec<T>,

    /// SCAN redis cursor. None if this is the first invocation of SCAN and no
    /// cursor exists yet.
    redis_cursor: Option<u64>,
}

impl <'a, T: FromRedisValue> RedisScanStream<'a, T> {
    /// Creates a new RedisScanStream.
    fn new(
        redis_conn: &'a mut MultiplexedConnection,
        match_arg: Option<String>
    ) -> RedisScanStream<'a, T>
    {
        RedisScanStream{
            redis_conn: redis_conn,
            match_arg: match_arg,
            items: Vec::new(),
            redis_cursor: None,
        }
    }
    
    /// Pops items off the items vector until it is empty. Then invokes the SCAN
    /// Redis command again to refill this vector until no more results
    /// are returned.
    async fn next(&mut self) -> Option<Result<T, String>> {
        // Try to get more items from the vector.
        match self.items.pop() {
            Some(v) => return Some(Ok(v)),
            _ => (),
        };

        // If no items and cursor is 0, stream is done.
        if self.redis_cursor == Some(0) {
            return None;
        }

        // Refill items with the results of the SCAN command.
        let cursor_arg = match self.redis_cursor {
            Some(v) => v,
            None => 0,
        };
        
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor_arg);

        if let Some(match_arg_val) = &self.match_arg {
            cmd.arg("MATCH").arg(match_arg_val);
        }

        let (new_cursor, mut new_items): (u64, Vec<T>) = match cmd
            .query_async(self.redis_conn).await
        {
            Err(e) => return Some(Err(
                format!("Failed to scan for keys: {}", e))),
            Ok(v) => v,
        };

        self.redis_cursor = Some(new_cursor);
        self.items.append(&mut new_items);

        // Finally used newly refilled items to return
        match self.items.pop() {
            Some(v) => Some(Ok(v)),
            None => None,
        }
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
    timestamp_seconds: i64,

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
#[derive(Serialize,Deserialize)]
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
        format!("sync_session:user:{}:{}", self.sync_session_id, self.id)
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

/// Response which all 5xx responses use.
#[derive(Serialize)]
struct ServerErrorResp<'a> {
    /// Public error message.
    error: &'a str,
}

impl <'a> ServerErrorResp<'a> {
    fn new(public_error: &'a str, internal_error: &'a str) -> ServerErrorResp<'a> {
        error!("public error={}, internal error={}", public_error, internal_error);
        ServerErrorResp{
            error: public_error,
        }
    }
}

/// Response which all 4xx responses use.
#[derive(Serialize)]
struct UserErrorResp<'a> {
    /// User error message.
    error: &'a str,
}

impl <'a> UserErrorResp<'a> {
    fn new(error: &'a str) -> UserErrorResp<'a> {
        UserErrorResp{
            error: error,
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
        .store(&sess).expire(&sess, REDIS_KEY_TTL)
        .store(&user).expire(&user, REDIS_KEY_TTL)
        .execute(&mut data.redis_conn.lock().unwrap()).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to save new sync session", &e)),
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

    /// Users who are part of the sync session.
    users: Vec<User>,
}

/// Retrieves a sync session by ID.
async fn get_sync_session(
    data: web::Data<AppState>,
    urldata: web::Path<String>
) -> impl Responder
{
    let redis_conn = &mut data.redis_conn.lock().unwrap();
    
    // Load sync session
    let sess_key = SyncSession::new_for_key(urldata.into_inner());
    
    let sess = match load_from_redis(redis_conn, &sess_key).await {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to get sync session", &e)),
        Ok(v) => v,
    };

    // Retrieve keys of users in sync session
    let sess_users_key = User::new_for_key(String::from(&sess_key.id),
                                           String::from("*"));
    let mut sess_users_keys: Vec<String> = Vec::new();

    let mut redis_scan = RedisScanStream::<String>::new(
        redis_conn, Some(sess_users_key.key()));

    while let Some(user_key_res) = redis_scan.next().await {
        let user_key_str = match user_key_res {
            Err(e) => return HttpResponse::InternalServerError().json(
                ServerErrorResp::new("Failed to find users in sync session", &e)),
            Ok(v) => v,
        };

        sess_users_keys.push(user_key_str);
    }

    // Retrieve users in sync session
    let mut sess_users: Vec<User> = Vec::new();

    for user_key_str in &sess_users_keys {
        let user_key = StringRedisKey::new(String::from(user_key_str.as_str()));
        
        let user = match load_from_redis(redis_conn, &user_key).await {
            Err(e) => return HttpResponse::InternalServerError().json(
                ServerErrorResp::new("Failed to retrieve information about one of\
                                      the users in the sync session",
                                     &format!("Failed to get {}: {}",
                                              &user_key_str, &e))),
            Ok(v) => match v {
                Some(some_v) => some_v,
                None => return HttpResponse::InternalServerError().json(
                    ServerErrorResp::new(
                        "Failed to retrieve information about one of the users \
                         in the sync session",
                        &format!("The user {} does not exist in Redis but the key \
                                  was found via SCAN", user_key_str))),
            },
        };
        sess_users.push(user);
    }

    // Send result
    match sess {
        None => HttpResponse::NotFound().json(
            UserErrorResp::new(&format!("Sync session {} not found",
                                        &sess_key.id))),
        Some(v) => HttpResponse::Ok().json(GetSyncSessionResp{
            sync_session: v,
            users: sess_users,
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

    let mut sess: SyncSession = match
        load_from_redis(&mut data.redis_conn.lock().unwrap(), &sess_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Error finding sync session to update",
                           &format!("Failed to get sync session: {}", e))),
        Ok(v) => match v {
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("Sync session {} not found",
                                            sess_key.id))),
            Some(v) => v,
        },
    };

    if let Some(new_name) = &req.name {
        sess.name = String::from(new_name);
    } else {
        return HttpResponse::BadRequest().json(
            UserErrorResp::new("Request must contain at least one field \
                                to update"));
    }

    sess.last_updated = Utc::now().timestamp();

    let mut redis_pipeline = redis::pipe();
    redis_pipeline.atomic();

    match RedisStoreMany::new(&mut redis_pipeline)
        .store(&sess).expire(&sess, REDIS_KEY_TTL)
        .execute(&mut data.redis_conn.lock().unwrap()).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to update sync session", &e)),
        _ => (),
    }

    HttpResponse::Ok().json(UpdateSyncSessMetaResp{
        sync_session: sess,
    })
}

/// Update sync session status request. See SyncSession field documentation for
/// more information on request fields.
#[derive(Deserialize)]
struct UpdateSyncSessionStatusReq {
    /// New sync session playing field.
    playing: bool,

    /// New sync session timestamp_seconds field.
    timestamp_seconds: i64,

    /// New sync session timestamp_last_updated field. 
    timestamp_last_updated: i64,
}

/// Update sync session status response.
#[derive(Serialize)]
struct UpdateSyncSessionStatusResp {
    sync_session: SyncSession,
}

/// Updates a sync session's playback status. Triggers sending an update message
/// on the play status web socket.
async fn update_sync_session_status(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: web::Json<UpdateSyncSessionStatusReq>,
) -> impl Responder
{
    // Get session to update
    let mut sess = SyncSession::new_for_key(urldata.to_string());

    sess = match load_from_redis(&mut data.redis_conn.lock().unwrap(),
                                 &sess).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to retrieve sync session to update", &e)),
        Ok(v) => match v {
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("Sync session {} not found",
                                            &sess.id))),
            Some(s) => s,
        },
    };

    // Update and store
    if &req.timestamp_last_updated <= &sess.timestamp_last_updated {
        return HttpResponse::Conflict().json(
            UserErrorResp::new(&format!("Sync session status has been updated \
                                         more recently than the submitted \
                                         request at {}",
                                        &req.timestamp_last_updated)));
    }
    
    sess.playing = req.playing;
    sess.timestamp_seconds = req.timestamp_seconds;
    sess.timestamp_last_updated = req.timestamp_last_updated;

    match store_in_redis(&mut data.redis_conn.lock().unwrap(), &sess).await {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to save updated sync session", &e)),
        _ => (),
    };

    // TODO: Trigger update

    HttpResponse::Ok().json(UpdateSyncSessionStatusResp{
        sync_session: sess,
    })
}

/// Default handler when no registered routes match a request.
async fn not_found(_req: HttpRequest) -> impl Responder
{
    HttpResponse::NotFound().json(UserErrorResp::new("Not found"))
}

fn on_bad_request<B>(
    mut res: ServiceResponse<B>
) -> actix_web::Result<ErrorHandlerResponse<B>>
{
    let error_str: String = match res.response().error() {
        Some(e) => format!("{}", e),
        None => String::from("Bad request"),
    };
    
    let resp_str = match serde_json::to_string(&UserErrorResp::new(&error_str)) {
        Err(e) => {
            error!("Failed to serialize bad request error response, tried to \
                    serialize={}, serialize error={}", error_str, &e);
            
                String::from("{\"error\": \"There was an error with your \
                              request, but while handling this an internal server \
                              error occurred\"}")
        },
        Ok(v) => v,
    };

    res.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json")
    );

    let new_res = res.map_body(|_head, _body| {
        ResponseBody::<B>::Other(actix_web::dev::Body::Bytes(
            actix_web::web::Bytes::from(resp_str)))
    });

    Ok(ErrorHandlerResponse::Response(new_res))
}

/// Creates a plain and async Redis connection and sends a ping commands to test
/// these connections.
async fn new_redis_connection() -> Result<MultiplexedConnection, String>
{
    let redis_client = match redis::Client::open("redis://127.0.0.1/") {
        Err(e) => return Err(format!("Failed to open redis connection: {}", e)),
        Ok(v) => v,
    };
    
    let (mut redis_conn, redis_driver) = match
        redis_client.get_multiplexed_async_connection().await
    {
        Err(e) => return Err(format!("Failed to get redis connection: {}", e)),
        Ok(v) => v,
        
    };

    actix_rt::spawn(redis_driver);

    let ping_res: RedisResult<String> = redis::cmd("PING")
        .query_async(&mut redis_conn).await;
    if ping_res != Ok("PONG".to_string()) {
        return Err(format!("Redis ping connection test failed: {:?}", ping_res));
    }

    Ok(redis_conn)
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    // Setup logger
    env_logger::from_env(Env::default().default_filter_or("debug")).init();
    
    // Connect to Redis
    let redis_conn = match new_redis_connection().await {
        Err(e) => return Err(std::io::Error::new(
            std::io::ErrorKind::Other, format!("Failed to connect to Redis: {}",
                                               e))),
        Ok(v) => v,
    };

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
            .route("/api/v0/sync_session/{id}/metadata", web::put()
                   .to(update_sync_session_metadata))
            .route("/api/v0/sync_session/{id}/status", web::put()
                   .to(update_sync_session_status))
            .default_service(web::route().to(not_found))
            .wrap(Logger::default())
            .wrap(ErrorHandlers::new()
                  .handler(http::StatusCode::BAD_REQUEST, on_bad_request))
    })
        .bind("0.0.0.0:8000")?
        .run()
        .await
}
