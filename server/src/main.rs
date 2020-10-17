use serde_json;
use serde::{Serialize,Deserialize};
use serde::de::DeserializeOwned;

use actix::{Actor, StreamHandler};
use actix_web_actors::ws;
use actix_web::{web,http,HttpServer,App,Responder,HttpRequest,HttpResponse};
use actix_web::dev::{ServiceResponse,ResponseBody};
use actix_web::middleware::Logger;
use actix_web::middleware::errhandlers::{ErrorHandlerResponse,ErrorHandlers};

use redis::{RedisResult,AsyncCommands,FromRedisValue};
use redis::aio::{ConnectionLike,MultiplexedConnection};

#[macro_use] extern crate log;
use env_logger::Env;
use uuid::Uuid;
use chrono::Utc;

use std::collections::HashMap;
use std::sync::{Arc,Mutex,RwLock,RwLockReadGuard,RwLockWriteGuard,PoisonError};
use std::marker::Sized;
use std::ops::Drop;
use std::convert::From;
use std::fmt;
use std::fmt::Debug;
use std::clone::Clone;
use std::rc::Rc;
use std::borrow::BorrowMut;
use std::cell::RefCell;

/// Number of seconds sync session and user keys last in Redis. Set to 1 day.
const REDIS_KEY_TTL: usize = 86400;

/// Initial Redis connection pool max connections value.
const REDIS_POOL_START_MAX_CONNS: u64 = 100;

/// Redis asynchronous connection pool.
///
/// Cloning the pool creates a new handle to the same connection pool. All
/// operations are thread safe.
///
/// ## Behavior
///
/// - get() creates a new Redis connection or uses an idle one if available.
/// - get() asynchronously blocks if the maximum number of connections has
///   been reached.
/// - get() returns a RedisConnectionLease which uses the Drop trait to return
///   connections to an idle connection pool when they go out of scope.
/// - If RedisConnectionLease::drop() fails the connection pool's state will be
///   corrupted and can no longer be used. Similar to std::sync::Mutex.
/// - The idle connection pool holds at most twice the number of leased connections.
/// - TODO: If there are 10 callers of get() waiting for a connection because the number
///   of maximum connections has been reached the pool will query Redis to see if it
///   can afford to increase the maximum number of connections
#[derive(Clone)]
struct RedisConnectionPool {
    /// Redis client which is used to open new connections.
    redis_client: redis::Client,

    /// Connection used for pool administration.
    admin_conn: MultiplexedConnection,

    /// Maximum number of allowed active connections. If this limit is frequently
    /// reached the pool will query Redis to see if there is spare space for
    /// more connections. If so the max_conns value will be raised.
    max_conns: u64,

    /// Number of connections which are currently leased.
    leased_conns: Arc<RwLock<u64>>,

    /// Number of callers which are waiting for a connection because the maximum
    /// number of connections has been reached.
    blocked_callers: Arc<RwLock<u64>>,

    /// List of idle clients. 
    idle_conns: Arc<RwLock<Vec<Arc<RefCell<MultiplexedConnection>>>>>,

    /// Stores an error which might occur while returning a connection to the
    /// pool. If this is not None then the pool is considered poisoned and cannot
    /// lease any new connections.
    end_lease_err: Arc<RwLock<Option<RedisPoolError>>>,
}

impl RedisConnectionPool {
    /// Initializes a connection pool.
    async fn new(redis_uri: &str) -> Result<RedisConnectionPool, String> {
        // Create client
        let redis_client = match redis::Client::open(redis_uri) {
            Err(e) => return Err(format!("Failed to open redis connection: {}", e)),
            Ok(v) => v,
        };

        // Create administration connection
        let admin_conn = match RedisConnectionPool::build_connection(
            &redis_client).await
        {
            Err(e) => return Err(format!("Failed to build administration \
                                          connection: {}", e)),
            Ok(v) => v,
        };

        Ok(RedisConnectionPool{
            redis_client: redis_client,
            admin_conn: admin_conn,
            max_conns: REDIS_POOL_START_MAX_CONNS,
            leased_conns: Arc::new(RwLock::new(0)),
            blocked_callers: Arc::new(RwLock::new(0)),
            idle_conns: Arc::new(RwLock::new(Vec::new())),
            end_lease_err: Arc::new(RwLock::new(None)),
        })
    }

    /// Used internally to create new asynchronous Redis connections. Pings Redis
    /// with the new connection before returning to make sure the connection
    /// is working.
    async fn build_connection(
        redis_client: &redis::Client
    ) -> Result<MultiplexedConnection, String>
    {
        // Create
        let (mut redis_conn, redis_driver) = match
            redis_client.get_multiplexed_async_connection().await
        {
            Err(e) => return Err(format!("Failed to get redis connection: {}", e)),
            Ok(v) => v,
            
        };

        actix_rt::spawn(redis_driver);

        // Test
        let ping_res: RedisResult<String> = redis::cmd("PING")
            .query_async(&mut redis_conn).await;
        if ping_res != Ok("PONG".to_string()) {
            return Err(format!("Redis ping connection test failed: {:?}",
                               ping_res));
        }

        Ok(redis_conn)
    }

    /// Builds a new RedisConnectionLease.
    fn build_conn_lease<'a>(
        &'a self,
        conn: Arc<RefCell<MultiplexedConnection>>
    ) -> RedisConnectionLease<'a>
    {
        RedisConnectionLease::new(self, conn)
    }

    /// Retrieve a connection for use. If there are idle connections available one
    /// of these is returned. If no idle connections are available and the pool is
    /// under the maximum connections limit a new connection is created. If no idle
    /// connections are available but the pool is at the maximum connection limit
    /// the method blocks asynchronously until a new connection is available.
    ///
    /// The connection is returned to the pool when the return value goes out
    /// of scope. See RedisConnectionLease for the implementation of this behavior.
    async fn get<'a>(
        &'a self,
    ) -> Result<RedisConnectionLease<'a>, RedisPoolError>
    {
        // Check if pool is poisoned, if so exit early
        match &*self.end_lease_err.read()? {
            Some(e) => return Err(RedisPoolError::new(&format!(
                "Connection pool is poisoned: {}", &e))),
            None => (),
        };
        
        // Check if there are any spare idle connections
        match self.idle_conns.write()?.pop() {
            Some(conn) => {
                let mut leased_conns = self.leased_conns.write()?;
                *leased_conns += 1;

                return Ok(self.build_conn_lease(conn));
            },
            None => (),
        };

        // If no avaliable idle connections check if we can build a new one
        if *self.leased_conns.read()? < self.max_conns {
            let conn = match Self::build_connection(&self.redis_client).await {
                Err(e) => return Err(RedisPoolError::new(&format!(
                    "Failed to build new Redis connection: {}", e))),
                Ok(v) => v,
            };

            let mut leased_conns = self.leased_conns.write()?;
            *leased_conns += 1;
            return Ok(self.build_conn_lease(Arc::new(RefCell::new(conn))));
        }

        // If out of idle connections and we can't build a new one, wait until
        // a connection becomes available
        // TODO: Implement connection waiting
        Err(RedisPoolError::new("Ran out of connections and haven't implemented \
                                 connection waiting yet"))
    }

    /// Returns a leased connection to the pool. Not called directly, but instead
    /// called by end_lease.
    fn try_end_lease(
        &self,
        conn: Arc<RefCell<MultiplexedConnection>>
    ) -> Result<(), RedisPoolError>
    {
        // Record as returned from lease
        let mut leased_conns = self.leased_conns.write()?;
        *leased_conns -= 1;
        
        // Store in idle pool if maximum number of idle connections has not
        // been reached
        let idle_conns_r = self.idle_conns.read()?;
        let leased_conns_r = self.leased_conns.read()?;
        
        if (idle_conns_r.len() as u64) < (*leased_conns_r * 2) {
            self.idle_conns.write()?.push(conn);
        }
        
        // Otherwise let self.conn fall out of scope and be closed
        Ok(())
    }
    
    /// Returns a leased connection to the pool. Records errors internally since
    /// this method is called by RedisConnectionLease::drop() which cannot fail.
    /// If this method fails the connection pool is considered poisoned and cannot
    /// lease out any new connections. Due to the fact that failing corrupts the
    /// internal state of the pool.
    fn end_lease(&self, conn: Arc<RefCell<MultiplexedConnection>>) {
        match self.try_end_lease(conn) {
            Err(e) => {
                error!("Error while ending lease: {}", &e);

                let mut end_lease_err = match self.end_lease_err.write() {
                    Err(ee) => {
                        panic!("Error while ending lease, but then an error \
                                occurred while trying to record this error: {}",
                               ee);
                    },
                    Ok(v) => v,
                };
                *end_lease_err = Some(e);
            },
            _ => (),
        };
    }
}

/// Error for redis connection pool.
#[derive(Clone)]
struct RedisPoolError {
    e: String,
}

impl RedisPoolError {
    /// Creates a new RedisPoolError from a string.
    fn new(e: &str) -> RedisPoolError {
        RedisPoolError{
            e: String::from(e),
        }
    }
}

impl
    From<PoisonError<RwLockReadGuard<'_, Option<RedisPoolError>>>>
    for
    RedisPoolError
{
    /// Converts a Mutex lock error into a RedisPoolError.
    fn from(
        e: PoisonError<RwLockReadGuard<'_, Option<RedisPoolError>>>
    ) -> Self
    {
        RedisPoolError{
            e: format!("Failed to lock: {}", e),
        }
    }
}

impl From<PoisonError<RwLockReadGuard<'_, u64>>> for RedisPoolError {
    /// Converts a Mutex lock error into a RedisPoolError.
    fn from(e: PoisonError<RwLockReadGuard<'_, u64>>) -> Self {
        RedisPoolError{
            e: format!("Failed to lock: {}", e),
        }
    }
}

impl From<PoisonError<RwLockWriteGuard<'_, u64>>> for RedisPoolError {
    /// Converts a Mutex lock error into a RedisPoolError.
    fn from(e: PoisonError<RwLockWriteGuard<'_, u64>>) -> Self {
        RedisPoolError{
            e: format!("Failed to lock: {}", e),
        }
    }
}

impl
    From<PoisonError<RwLockReadGuard<'_, Vec<Arc<RefCell<MultiplexedConnection>>>>>>
    for RedisPoolError
{
    /// Converts a Mutex lock error into a RedisPoolError.
    fn from(
        e: PoisonError<RwLockReadGuard<'_, Vec<Arc<RefCell<MultiplexedConnection>>>>>
    ) -> Self
    {
        RedisPoolError{
            e: format!("Failed to lock: {}", e),
        }
    }
}

impl
    From<PoisonError<RwLockWriteGuard<'_, Vec<Arc<RefCell<MultiplexedConnection>>>>>>
    for RedisPoolError
{
    /// Converts a Mutex lock error into a RedisPoolError.
    fn from(
        e: PoisonError<RwLockWriteGuard<'_, Vec<Arc<RefCell<MultiplexedConnection>>>>>
    ) -> Self
    {
        RedisPoolError{
            e: format!("Failed to lock: {}", e),
        }
    }
}

impl fmt::Display for RedisPoolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.e)
    }
}

/// Leases a Redis connection from a RedisConnectionPool. Once it goes out of
/// scope the connection is returned to the pool. If an error occurs while
/// returning it is stored in the connection pool, since the Drop trait
/// cannot fail.
struct RedisConnectionLease<'a> {
    /// Connection pool from which underlying connection was leased.
    pool: &'a RedisConnectionPool,
    
    /// Underlying connection.
    conn: Arc<RefCell<MultiplexedConnection>>,
}

impl <'a> RedisConnectionLease<'a> {
    /// Creates a new connection lease.
    fn new(
        pool: &'a RedisConnectionPool,
        conn: Arc<RefCell<MultiplexedConnection>>,
    ) -> RedisConnectionLease<'a>
    {
        RedisConnectionLease{
            pool: pool,
            conn: conn,
        }
    }
}

impl <'a> Drop for RedisConnectionLease<'a> {
    /// Returns the underyling connection to the connection pool.
    fn drop(&mut self) {
        self.pool.end_lease(self.conn.clone());
    }
}

/// HTTP app state.
struct AppState {
    /// Redis connection pool.
    redis_pool: RedisConnectionPool,
    
    /// Asynchronous Redis connection.
    redis_conn: Mutex<MultiplexedConnection>,
}

/// Provides the key in Redis under which an object will be stored.
trait RedisKey where Self: Sized {
    /// Redis key for the specific item. The returned result can be used with the
    /// Redis SCAN command if the object's key fields are set to "*".
    fn key(&self) -> String;

    /// Create an instance of the implementing type who's field values are
    /// populated so calling key() would return the same redis key value which was
    /// passed as an argument.
    fn from_key(redis_key: String) -> Result<Self, String>;
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
        self.key.clone()
    }

    fn from_key(redis_key: String) -> Result<Self, String> {
        Ok(StringRedisKey::new(redis_key))
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

    /// Time to live values for key in Redis. Keys = Redis key, values = time to
    /// live in seconds.
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
    fn store<T: Serialize, K: RedisKey>(
        &mut self,
        key_obj: &K,
        data: &T,
    ) -> &mut Self
    {
        let key = key_obj.key();

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
    /// because setting a key clears any existing TTL.
    fn expire<T: RedisKey>(&mut self, key: &T, ttl: usize) -> &mut Self
    {
        self.ttls.insert(key.key(), ttl);

        self
    }

    /// Run store and expire commands on pipeline.
    async fn execute(
        &mut self,
        redis_conn: &mut MultiplexedConnection
    )-> Result<(), String>
    {
        // Check for serialization errors
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

        // Add SET and EXPIRE commands to pipeline
        for (key, data_json) in &self.data {
            self.redis_pipeline.cmd("SET").arg(key).arg(data_json);
        }

        for (key, ttl) in &self.ttls {
            self.redis_pipeline.cmd("EXPIRE").arg(key).arg(*ttl);
        }

        // Run pipeline
        match self.redis_pipeline.query_async::<MultiplexedConnection,Vec<String>>
            (redis_conn).await
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
#[derive(Serialize,Deserialize,Debug)]
struct SyncSession {
    /// Identifier, UUIDv4.
    id: String,

    /// Name of session.
    name: String,

    /// Indicates if users must be privileged to set playback metadata.
    privileged_playback: bool,

    /// If video is running.
    playing: bool,

    /// Number of seconds into video when it was the time specified
    /// by timestamp_last_updated.
    timestamp_seconds: i64,

    /// Client's time when they updated timestamp_seconds.
    timestamp_last_updated: i64,

    /// Last time any information about session was updated, including
    /// timestamp_seconds This time is the server's time. 
    last_updated: i64,
}

impl RedisKey for SyncSession {
    fn key(&self) -> String {
        format!("sync_session:{}", self.id)
    }

    fn from_key(redis_key: String) -> Result<Self, String> {
        let parts: Vec<&str> = redis_key.split(":").collect();

        if parts.len() != 2 {
            return Err(String::from("Provided redis key was invalid. Should be \
                                     in format \"sync_session:<ID>\""));
        }
        
        Ok(SyncSession::new_for_key(String::from(parts[1])))
    }
}

impl SyncSession {
    /// Creates a new sync session with only the id field populated. This allows
    /// the structure to function properly as a RedisKey. All other fields have
    /// empty values which should be replaced before use. 
    fn new_for_key(id: String) -> SyncSession {
        SyncSession{
            id: id,
            name: String::new(),
            privileged_playback: false,
            playing: false,
            timestamp_seconds: 0,
            timestamp_last_updated: 0,
            last_updated: 0,
        }
    }
}

/// User in a sync session.
/// Times are the number of non-leap seconds since EPOCH in UTC.
/// Exactly one sync session owner must exist in a sync session. Admins can
/// see other user's secret IDs. As a result they can rename and kick them
/// from the session. Admins cannot modify owners.
#[derive(Serialize,Deserialize,Debug)]
struct User {
    /// Secret identifier of user, UUIDv4. If a client has this ID they can make
    /// changes to the user. Thus only the user themselves and admins know this ID.
    #[serde(skip)]
    id: String,

    /// Public identifier of user. Everyone knows this ID but having it does not
    /// grant any control over the user.
    public_id: String,

    /// Identifier of sync session user belongs to.
    sync_session_id: String,
    
    /// Friendly name to identify user.
    name: String,

    /// Authorization role attached to user. Can be the values of USER_ROLE_OWNER,
    /// USER_ROLE_ADMIN, or empty.
    role: String,

    /// Last time the client was seen from the server's perspective.
    last_seen: i64,
}

/// Indicates a user is an owner.
const USER_ROLE_OWNER: &str = "owner";

/// Indicates a user is an admin.
const USER_ROLE_ADMIN: &str = "admin";

impl RedisKey for User {
    fn key(&self) -> String {
        format!("sync_session:user:{}:{}", self.sync_session_id, self.id)
    }

    fn from_key(redis_key: String) -> Result<Self, String> {
        let parts: Vec<&str> = redis_key.split(":").collect();

        if parts.len() != 4 {
            return Err(String::from("Provided redis key was invalid. Must be in \
                                     the format \
                                     \"sync_session:user:\
                                     <sync session ID>:<user ID>\""));
        }
        
        Ok(User::new_for_key(String::from(parts[2]),
                             String::from(parts[3])))
    }
}

impl User {
    /// Creates a user with the id and sync_session_id fields populated. This
    /// allows the structure to function properly as a RedisKey. All other fields
    /// are given empty values and should be replaced before use.
    fn new_for_key(sync_session_id: String, id: String) -> User {
        User{
            id: id,
            public_id: String::new(),
            sync_session_id: sync_session_id,
            name: String::new(),
            role: String::new(),
            last_seen: 0,
        }
    }

    /// Returns true if the user's role gives them privileged access.
    fn is_privileged(&self) -> bool {
        self.role == USER_ROLE_OWNER || self.role == USER_ROLE_ADMIN
    }
}

/// Response which all 5xx responses use.
#[derive(Serialize)]
struct ServerErrorResp {
    /// Public error message.
    error: String
}

impl ServerErrorResp {
    fn new(public_error: &str, internal_error: &str) -> ServerErrorResp {
        error!("public error={}, internal error={}", public_error, internal_error);
        ServerErrorResp{
            error: String::from(public_error),
        }
    }
}

/// Response which all 4xx responses use.
#[derive(Serialize)]
struct UserErrorResp {
    /// User error message.
    error: String
}

impl UserErrorResp {
    fn new(error: &str) -> UserErrorResp {
        UserErrorResp{
            error: String::from(error),
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

    /// Secret ID of user.
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
        privileged_playback: false,
        playing: false,
        timestamp_seconds: 0,
        timestamp_last_updated: now,
        last_updated: now,
    };

    let user = User{
        id: Uuid::new_v4().to_string(),
        public_id: Uuid::new_v4().to_string(),
        sync_session_id: sess.id.clone(),
        name: String::from("Owner"),
        role: String::from(USER_ROLE_OWNER),
        last_seen: now,
    };

    let mut redis_pipeline = redis::pipe();
    redis_pipeline.atomic();

    match RedisStoreMany::new(&mut redis_pipeline)
        .store(&sess, &sess).expire(&sess, REDIS_KEY_TTL)
        .store(&user, &user).expire(&user, REDIS_KEY_TTL)
        .execute(&mut data.redis_conn.lock().unwrap()).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to save new sync session", &e)),
        _ => (),
    };
    
    HttpResponse::Ok().json(CreateSyncSessionResp{
        sync_session: sess,
        user_id: user.id.clone(),
        user: user,
    })
}

/// Get sync session endpoint response.
#[derive(Serialize)]
struct GetSyncSessionResp {
    /// Retrieved sync session.
    sync_session: SyncSession,

    /// Users who are part of the sync session. Keys are public User IDs.
    users: HashMap<String, User>,

    /// Mapping between public user IDs and private user IDs. Keys are user public
    /// IDs. Values are user private IDs. Only returned if the user ID in the
    /// request's Authorization header is an owner or admin. The owner's private ID
    /// is never returned.
    user_ids: Option<HashMap<String, String>>,
}

/// Retrieves a sync session by ID. A user ID can be passed in the Authorization
/// header to retrieve privileged information.
async fn get_sync_session(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: HttpRequest,
) -> impl Responder
{
    let redis_conn = &mut data.redis_conn.lock().unwrap();
    let sess_id = urldata.into_inner();

    // Retrieve authorized user if provided
    let authorized_user = match get_authorized_user(&req, redis_conn,
                                                    sess_id.clone()).await
    {
        Err(e) => match e {
            GetAuthUserError::ServerError(e) =>
                return HttpResponse::InternalServerError().json(e),
            GetAuthUserError::NotFound(e) =>
                return HttpResponse::NotFound().json(e),
        },
        Ok(v) => v,
    };

    let authorized_user_privileged = match authorized_user {
        Some(user) => user.is_privileged(),
        None => false,
    };


    // Load sync session
    let sess_key = SyncSession::new_for_key(sess_id.clone());

    let sess: SyncSession = match load_from_redis(redis_conn, &sess_key).await {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to get sync session", &e)),
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("Sync session {} not found",
                                            &sess_key.id))),
        },
    };

    // Retrieve keys of users in sync session
    let sess_users_key = User::new_for_key(sess_key.id.clone(),
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
    let mut sess_users: HashMap<String, User> = HashMap::new();
    let mut user_ids_mapping: HashMap<String, String> = HashMap::new();

    for user_key_str in &sess_users_keys {
        let user_key = StringRedisKey::new(user_key_str.clone());
        
        let user: User = match load_from_redis(redis_conn, &user_key).await {
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

        let user_public_id = user.public_id.clone();
        let user_role = user.role.clone();
        
        sess_users.insert(user_public_id.clone(), user);

        if authorized_user_privileged && user_role != USER_ROLE_OWNER {
            let private_user_id = match User::from_key((*user_key_str).clone()) {
                Err(e) => return HttpResponse::InternalServerError().json(
                    ServerErrorResp::new(
                        "Failed to process users in sync session",
                        &format!("Failed to convert raw Redis key \"{}\" \
                                  into User struct: {}",
                                 (*user_key_str).clone(), e))),
                Ok(v) => v.id,
            };
            
            user_ids_mapping.insert(user_public_id.clone(), private_user_id);
        }
    }

    // Send result
    HttpResponse::Ok().json(GetSyncSessionResp{
        sync_session: sess,
        users: sess_users,
        user_ids: match authorized_user_privileged {
            true => Some(user_ids_mapping),
            false => None,
        },
    })
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
        sess.name = new_name.clone();
    } else {
        return HttpResponse::BadRequest().json(
            UserErrorResp::new("Request must contain at least one field \
                                to update"));
    }

    sess.last_updated = Utc::now().timestamp();

    let mut redis_pipeline = redis::pipe();
    redis_pipeline.atomic();

    match RedisStoreMany::new(&mut redis_pipeline)
        .store(&sess, &sess).expire(&sess, REDIS_KEY_TTL)
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

/// Update sync session privileged metadata request.
#[derive(Deserialize)]
struct UpdateSyncSessPrivilegedMetaReq {
    /// New set playback required privileges field value.
    privileged_playback: bool,
}

/// Update sync session privileged metadata response.
#[derive(Serialize)]
struct UpdateSyncSessPrivilegedMetaResp {
    /// Updated sync session.
    sync_session: SyncSession,
}

/// Updates a sync session's privileged metadata.
async fn update_sync_session_privileged_metadata(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: HttpRequest,
    req_body: web::Json<UpdateSyncSessPrivilegedMetaReq>,
) -> impl Responder
{
    let redis_conn = &mut data.redis_conn.lock().unwrap();
    let sess_key = SyncSession::new_for_key(urldata.into_inner());

    // Retrieve authorized user if provided
    let authorized_user = match get_authorized_user(&req, redis_conn,
                                                    sess_key.id.clone()).await
    {
        Err(e) => match e {
            GetAuthUserError::ServerError(e) =>
                return HttpResponse::InternalServerError().json(e),
            GetAuthUserError::NotFound(e) =>
                return HttpResponse::NotFound().json(e),
        },
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::Unauthorized().json(
                UserErrorResp::new("User not found")),
        },
    };

    if !authorized_user.is_privileged() {
        return HttpResponse::Unauthorized().json(
            UserErrorResp::new("You do not have permissions to complete \
                                this action"));
    }

    // Get sync session to update
    let mut sess: SyncSession = match
        load_from_redis(redis_conn, &sess_key).await
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


    // Update
    sess.privileged_playback = req_body.privileged_playback;
    sess.last_updated = Utc::now().timestamp();

    let mut redis_pipeline = redis::pipe();
    redis_pipeline.atomic();

    match RedisStoreMany::new(&mut redis_pipeline)
        .store(&sess, &sess).expire(&sess, REDIS_KEY_TTL)
        .execute(redis_conn).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to update sync session", &e)),
        _ => (),
    }

    HttpResponse::Ok().json(UpdateSyncSessPrivilegedMetaResp{
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
/// on the play status web socket. Authorization may be required if the sync
/// session's privileged_playback field is true.
async fn update_sync_session_playback(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: HttpRequest,
    req_data: web::Json<UpdateSyncSessionStatusReq>,
) -> impl Responder
{
    let redis_conn = &mut data.redis_conn.lock().unwrap();
    
    // Get session to update
    let mut sess = SyncSession::new_for_key(urldata.to_string());

    sess = match load_from_redis(redis_conn,
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

    // Authenticate user if required
    if sess.privileged_playback {
        match get_authorized_user(&req, redis_conn, sess.id.clone()).await {
            Err(e) => match e {
                GetAuthUserError::ServerError(e) =>
                    return HttpResponse::InternalServerError().json(e),
                GetAuthUserError::NotFound(e) =>
                    return HttpResponse::NotFound().json(e),
            },
            Ok(v) => match v {
                Some(user) => {
                    if !user.is_privileged() {
                        return HttpResponse::Unauthorized().json(
                            UserErrorResp::new("You do not have permission to \
                                                perform this action"));
                    }
                },
                None => {
                    return HttpResponse::Unauthorized().json(
                        UserErrorResp::new("User with ID provided in \
                                            Authorization header does not exist"));
                },
            },
        };
    }

    // Update and store
    if &req_data.timestamp_last_updated <= &sess.timestamp_last_updated {
        return HttpResponse::Conflict().json(
            UserErrorResp::new(&format!("Sync session status has been updated \
                                         more recently than the submitted \
                                         request at {}",
                                        &req_data.timestamp_last_updated)));
    }
    
    sess.playing = req_data.playing;
    sess.timestamp_seconds = req_data.timestamp_seconds;
    sess.timestamp_last_updated = req_data.timestamp_last_updated;
    sess.last_updated = Utc::now().timestamp();

    let mut pipeline = redis::pipe();
    pipeline.atomic();

    match RedisStoreMany::new(&mut pipeline)
        .store(&sess, &sess).expire(&sess, REDIS_KEY_TTL)
        .execute(redis_conn).await {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to save updated sync session", &e)),
        _ => (),
    };

    // TODO: Trigger update message on web socket

    HttpResponse::Ok().json(UpdateSyncSessionStatusResp{
        sync_session: sess,
    })
}

/// Request for join sync session endpoint.
#[derive(Deserialize)]
struct JoinSyncSessionReq {
    /// Name of user joining.
    name: String,
}

/// Response of join sync session endpoint.
#[derive(Serialize)]
struct JoinSyncSessionResp {
    /// Sync session being joined.
    sync_session: SyncSession,
    
    /// User created for client who is joining.
    user: User,

    /// ID of client's user.
    user_id: String,
}

/// Creates a new user in the specified sync session.
async fn join_sync_session(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: web::Json<JoinSyncSessionReq>,
) -> impl Responder
{
    // Get sync session
    let sess_key = SyncSession::new_for_key(urldata.into_inner());

    let mut sess: SyncSession = match load_from_redis(
        &mut data.redis_conn.lock().unwrap(), &sess_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to get sync session", &e)),
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("Sync session {} not found",
                                            &sess_key.id))),
        },
    };

    // Create user
    let user = User{
        id: Uuid::new_v4().to_string(),
        public_id: Uuid::new_v4().to_string(),
        sync_session_id: sess.id.clone(),
        name: req.name.clone(),
        role: String::new(),
        last_seen: Utc::now().timestamp(),
    };

    // Store user and update sync session
    sess.last_updated = Utc::now().timestamp();

    let mut pipeline = redis::pipe();
    pipeline.atomic();

    match RedisStoreMany::new(&mut pipeline)
        .store(&user, &user).expire(&user, REDIS_KEY_TTL)
        .store(&sess, &sess).expire(&sess, REDIS_KEY_TTL)
        .execute(&mut data.redis_conn.lock().unwrap()).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to save user", &e)),
        _ => (),
    };

    // TODO: Trigger update on web socket

    HttpResponse::Ok().json(JoinSyncSessionResp{
        sync_session: sess,
        user_id: user.id.clone(),
        user: user,
    })
}

/// Request for the leave sync session endpoint.
#[derive(Deserialize)]
struct LeaveSyncSessionReq {
    /// ID of user to remove from sync session.
    user_id: String,
}

/// Response for the leave sync session endpoint.
#[derive(Serialize)]
struct LeaveSyncSessionResp {
    /// Indicates operation succeeded.
    ok: bool,
}

/// Removes a user from a sync session.
async fn leave_sync_session(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: web::Json<LeaveSyncSessionReq>
) -> impl Responder
{
    let redis_conn = &mut data.redis_conn.lock().unwrap();
    let url_sess_id = urldata.into_inner();
    
    // Check user exists
    let user_key = User::new_for_key(url_sess_id.clone(),
                                     req.user_id.clone());
    let user: User = match load_from_redis(redis_conn, &user_key).await {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to check if user exists",
                                 &format!("{}", &e))),
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("User {} not found", &req.user_id))),
        },
    };

    // Ensure user is not the owner, because there always has to be one owner in a
    // sync session. So before they leave they must transfer ownership to
    // someone else.
    if user.role == USER_ROLE_OWNER {
        return HttpResponse::Conflict().json(
            UserErrorResp::new("Cannot delete user because they are the owner \
                                of the sync session. Transfer ownership to \
                                someone else then try again."));
    }

    // Get sync session
    let sess_key = SyncSession::new_for_key(url_sess_id.clone());

    let mut sess: SyncSession = match load_from_redis(redis_conn, &sess_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to get sync session", &e)),
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("Sync session {} not found",
                                            &sess_key.id))),
        },
    };

    // Delete user
    sess.last_updated = Utc::now().timestamp();

    let mut pipeline = redis::pipe();
    pipeline.atomic();

    pipeline.cmd("DEL").arg(&user_key.key());

    match RedisStoreMany::new(&mut pipeline)
        .store(&sess, &sess).expire(&sess, REDIS_KEY_TTL)
        .execute(redis_conn).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to delete user", &e)),
        _ => (),
    };

    // TODO: Notify web socket

    HttpResponse::Ok().json(LeaveSyncSessionResp{
        ok: true,
    })
}

/// Request for the update sync session user endpoint.
#[derive(Deserialize)]
struct UpdateSyncSessionUserReq {
    /// ID of user.
    user_id: String,
    
    /// New name of the user.
    name: String,
}

/// Response for the update sync session user endpoint.
#[derive(Serialize)]
struct UpdateSyncSessionUserResp {
    /// Updated user.
    user: User,
}

/// Updates a user in the specified sync session.
async fn update_sync_session_user(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: web::Json<UpdateSyncSessionUserReq>,
) -> impl Responder
{
    let url_sess_id = urldata.into_inner();

    // Get user
    let user_key = User::new_for_key(url_sess_id.clone(),
                                     req.user_id.clone());
    let mut user: User = match load_from_redis(
        &mut data.redis_conn.lock().unwrap(), &user_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to get user to update", &e)),
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("User in sync session {} with id {} \
                                             was not found",
                                            &url_sess_id, &req.user_id))),
        },
    };

    // Get sync session
    let sess_key = SyncSession::new_for_key(url_sess_id.clone());
    
    let mut sess: SyncSession = match load_from_redis(
        &mut data.redis_conn.lock().unwrap(), &sess_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new(
                "Failed to get sync session associated with user", &e)),
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("Sync session {} which is associated \
                                             with the user {} was not found",
                                            &url_sess_id, &req.user_id))),
        },
    };

    // Update user
    user.id = req.user_id.clone();
    user.name = req.name.clone();
    user.last_seen = Utc::now().timestamp();

    sess.last_updated = Utc::now().timestamp();
    
    let mut redis_pipeline = redis::pipe();
    redis_pipeline.atomic();

    match RedisStoreMany::new(&mut redis_pipeline)
        .store(&user, &user).expire(&user, REDIS_KEY_TTL)
        .store(&sess, &sess).expire(&sess, REDIS_KEY_TTL)
        .execute(&mut data.redis_conn.lock().unwrap()).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to save updated user", &e)),
        _ => (),
    };

    // TODO: Trigger update on web socket

    HttpResponse::Ok().json(UpdateSyncSessionUserResp{
        user: user,
    })
}

/// Request for update sync session user role endpoint.
#[derive(Deserialize)]
struct UpdateSyncSessUserRoleReq {
    /// ID of user for which role will be set.
    user_id: String,

    /// New role value for user.
    role: String,
}

/// Response of update sync session user role endpoint.
#[derive(Serialize)]
struct UpdateSyncSessUserRoleResp {
    /// New state of requester user.
    requester_user: User,

    /// New state of target user.
    target_user: User,
}

/// Update a user's role. A user ID must be passed in the Authorization header
/// to authenticate. Only owners or admins can change user roles. Only owners can
/// make another user an owner, and doing so makes the requester an admin.
async fn update_sync_session_user_role(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: HttpRequest,
    req_body: web::Json<UpdateSyncSessUserRoleReq>,
) -> impl Responder
{
    let redis_conn = &mut data.redis_conn.lock().unwrap();
    let sess_key = SyncSession::new_for_key(urldata.into_inner());

    // Get authorized user
    let mut authorized_user = match get_authorized_user(&req, redis_conn,
                                                        sess_key.id.clone()).await
    {
        Err(e) => match e {
            GetAuthUserError::ServerError(e) =>
                return HttpResponse::InternalServerError().json(e),
            GetAuthUserError::NotFound(e) =>
                return HttpResponse::NotFound().json(e),
        },
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::Unauthorized().json(
                UserErrorResp::new("Failed to authenticate, user with ID does \
                                    not exist")),
        },
    };

    // Ensure user is privileged
    if !authorized_user.is_privileged() {
        return HttpResponse::Unauthorized().json(
            UserErrorResp::new("You do not have the proper permissions to \
                                perform this action"));
    }

    // Ensure request body role value is valid
    if &req_body.role != USER_ROLE_OWNER && &req_body.role != USER_ROLE_ADMIN &&
        &req_body.role != ""
    {
        return HttpResponse::BadRequest().json(
            UserErrorResp::new(&format!("role field must be either \"{}\", \
                                         \"{}\", \"\"", USER_ROLE_OWNER,
                                        USER_ROLE_ADMIN)));
    }

    // Ensure that the requester is not an owner tweaking their own role. As this
    // could leave a sync session without an owner.
    if &authorized_user.role == USER_ROLE_OWNER &&
        &req_body.user_id == &authorized_user.id
    {
        return HttpResponse::BadRequest().json(
            UserErrorResp::new("Owners cannot update their own role. If you are \
                                trying to remove your owner role you must make \
                                another user an owner first."));
    }

    // Ensure that if making a user an owner the requester is an owner
    if &req_body.role == USER_ROLE_OWNER &&
        &authorized_user.role != USER_ROLE_OWNER
    {
        return HttpResponse::Unauthorized().json(
            UserErrorResp::new("Only owners can promote users to owners"));
    }

    // Get sync session user belongs to
    let mut sess: SyncSession = match load_from_redis(redis_conn,
                                                      &sess_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to retrieve sync session which users \
                                  are part of", &e)),
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("Sync session {} not found",
                                            &sess_key.id))),
        },
    };

    // Get user who we are updating for which we are updating the role
    let update_user_key = User::new_for_key(sess_key.id.clone(),
                                            req_body.user_id.clone());
    let mut update_user: User = match load_from_redis(redis_conn,
                                                      &update_user_key).await
    {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to retrieve user to update", &e)),
        Ok(v) => match v {
            Some(some_v) => some_v,
            None => return HttpResponse::NotFound().json(
                UserErrorResp::new(&format!("User specified to update, with \
                                             ID {}, was not found",
                                            &req_body.user_id))),
        },
    };

    let mut pipeline = redis::pipe();
    pipeline.atomic();

    let mut store_many = RedisStoreMany::new(&mut pipeline);

    update_user.role = req_body.role.clone();

    store_many
        .store(&update_user_key, &update_user)
        .expire(&update_user_key, REDIS_KEY_TTL);

    // Update requester (who is an owner) if we are promoting someone to owner
    if &req_body.role == USER_ROLE_OWNER {
        authorized_user.role = String::from(USER_ROLE_ADMIN);

        store_many
            .store(&authorized_user, &authorized_user)
            .expire(&authorized_user, REDIS_KEY_TTL);
    }

    // Update session
    sess.last_updated = Utc::now().timestamp();
    
    store_many
        .store(&sess, &sess).expire(&sess, REDIS_KEY_TTL);

    // Execute updates
    match store_many.execute(redis_conn).await {
        Err(e) => return HttpResponse::InternalServerError().json(
            ServerErrorResp::new("Failed to update users", &e)),
        _ => (),
    };

    HttpResponse::Ok().json(UpdateSyncSessUserRoleResp{
        requester_user: authorized_user,
        target_user: update_user,
    })
}

/// Converts a connect to a web socket. This web socket will send a message to
/// the client whenever a change occurs in the sync session.
async fn changes_sync_session_web_socket(
    data: web::Data<AppState>,
    urldata: web::Path<String>,
    req: HttpRequest,
    stream: web::Payload,
) -> impl Responder
{
    ws::start(SyncSessionChangesWS{
        sync_session_id: urldata.into_inner(),
        app_state: data,
    }, &req, stream)
}

/// Web socket which notifies clients when a change occurs in a sync session.
struct SyncSessionChangesWS {
    /// ID of sync session for which to notify changes.
    sync_session_id: String,

    /// Redis connection.
    app_state: web::Data<AppState>,
}

impl Actor for SyncSessionChangesWS {
    type Context = ws::WebsocketContext<Self>;
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>>
    for SyncSessionChangesWS
{
    /// Handles web socket messages.
    fn handle(
        &mut self,
        msg: Result<ws::Message, ws::ProtocolError>,
        ctx: &mut Self::Context,
    ) {
        match msg {
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Text(text)) => ctx.text(format!("you said: {}", text)),
            _ => (),
        }
    }
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
    match res.headers().get(http::header::CONTENT_TYPE) {
        Some(header) => match header.to_str() {
            Err(e) => {
                error!("In bad request error middleware, failed to read response \
                        Content-Type header, will treat is if not set: {}", e);
            },
            Ok(v) => {
                if v == "application/json" {
                    return Ok(ErrorHandlerResponse::Response(res));
                }
                warn!("In bad request error middleware, got bad request response \
                       with content type={}, should always be application/json",
                      v);
            }
        },
        _ => (),
    };
    
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

/// Identifies error returned by get_authorized_user().
enum GetAuthUserError {
    ServerError(ServerErrorResp),
    NotFound(UserErrorResp),
}

/// Retrieves the user who's ID is in the Authorization header. The returned
/// user will have its id field set (which is unusual for users retrieved from
/// Redis) so that the caller can know the value of the Authoriation header.
async fn get_authorized_user(
    req: &HttpRequest,
    redis_conn: &mut MultiplexedConnection,
    sess_id: String,
) -> Result<Option<User>, GetAuthUserError>
{
    match req.headers().get(http::header::AUTHORIZATION) {
        Some(uid_raw) => {
            let uid = match uid_raw.to_str() {
                Err(e) => return Err(GetAuthUserError::ServerError(
                    ServerErrorResp::new(
                        "Failed to read Authorization header value",
                        &format!("{}", e)))),
                Ok(v) => v,
            };
            
            let user_key = User::new_for_key(sess_id.clone(),
                                             String::from(uid));
            
            match load_from_redis::<User, User>(redis_conn, &user_key).await {
                Err(e) => return Err(GetAuthUserError::ServerError(
                    ServerErrorResp::new(
                        "Failed to retrieve information about user associated \
                         with authorization credentials", &e))),
                Ok(v) => match v {
                    Some(mut some_v) => {
                        some_v.id = user_key.id.clone();
                        Ok(Some(some_v))
                    },
                    None => return Err(GetAuthUserError::NotFound(
                        UserErrorResp::new(&format!(
                        "User ID provided as authorization \"{}\" \
                         does not exist", uid)))),
                },
            }
        },
        None => Ok(None),
    }
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

    let redis_pool = match RedisConnectionPool::new("redis://127.0.0.1/").await {
        Err(e) => return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("Failed to create Redis connection pool: {}", e))),
        Ok(v) => v,
    };

    info!("Connected to Redis");

    // Start HTTP server
    let app_state = web::Data::new(AppState{
        redis_pool: redis_pool.clone(),
        redis_conn: Mutex::new(redis_conn),
    });

    // Just testing redis connection pool, will delete later
    let pool_conn = match redis_pool.get().await {
        Err(e) => panic!("failed to get pool conn: {}", e),
        Ok(v) => v,
    };
    
    let ping_res: RedisResult<String> = redis::cmd("PING")
        .query_async(pool_conn.conn).await;
    if ping_res != Ok("PONG".to_string()) {
        error!("Failed to ping during pool test");
    } else{
        debug!("Pool test success");
    }

    // TODO: Make subscribe web socket

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .default_service(web::route().to(not_found))
            .wrap(Logger::default())
            .wrap(ErrorHandlers::new()
                  .handler(http::StatusCode::BAD_REQUEST, on_bad_request))
            .route("/api/v0/status", web::get().to(server_status))
            .route("/api/v0/sync_session", web::post().to(create_sync_session))
            .route("/api/v0/sync_session/{id}", web::get().to(get_sync_session))
            .route("/api/v0/sync_session/{id}/metadata", web::put()
                   .to(update_sync_session_metadata))
            .route("/api/v0/sync_session/{id}/metadata/privileged", web::put()
                   .to(update_sync_session_privileged_metadata))
            .route("/api/v0/sync_session/{id}/playback", web::put()
                   .to(update_sync_session_playback))
            .route("/api/v0/sync_session/{id}/user", web::post()
                   .to(join_sync_session))
            .route("/api/v0/sync_session/{id}/user", web::delete()
                   .to(leave_sync_session))
            .route("/api/v0/sync_session/{id}/user", web::put()
                   .to(update_sync_session_user))
            .route("/api/v0/sync_session/{id}/user/role", web::put()
                   .to(update_sync_session_user_role))
            .route("/api/v0/sync_session/{id}/changes", web::get()
                   .to(changes_sync_session_web_socket))
    })
        .bind("127.0.0.1:8000")?
        .run()
        .await
}
