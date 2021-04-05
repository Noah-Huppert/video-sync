use std::fs;
use std::io;
use std::convert::From;

use serde_derive::{Serialize, Deserialize};
use toml;
use warp;
use warp::Filter;

/// Application configuration.
#[derive(Deserialize)]
struct Config {
    /// HTTP server configuration.
    server: ServerConfig,
}

/// Application server configuration.
#[derive(Deserialize)]
struct ServerConfig {
    /// HTTP port on which to listen.
    port: u16,
}

/// Configuration error.
#[derive(Debug)]
enum ConfigError {
    /// Error occurred while fetching configuration from the disk.
    IO(io::Error),

    /// Error occurred parsing texting to configuration.
    Parse(toml::de::Error),
}

impl From<io::Error> for ConfigError {
    fn from(err: io::Error) -> Self {
        ConfigError::IO(err)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(err: toml::de::Error) -> Self {
        ConfigError::Parse(err)
    }
}

impl Config {
    /// Load configuration from a TOML file.
    fn load_from_file(path: &str) -> Result<Config, ConfigError> {
        let contents = fs::read_to_string(path)?;
        let cfg = toml::from_str(&contents)?;
        Ok(cfg)
    }
}

/// Application error.
#[derive(Debug)]
enum AppError {
    /// If the configuration fails to load.
    Config(ConfigError),
}

impl From<ConfigError> for AppError {
    fn from(err: ConfigError) -> Self {
        AppError::Config(err)
    }
}

/// HTTP API server.
struct APIServer {
    /// Application configuration
    cfg: Config,
}

/// API error.
enum APIError {
}

/// Health endpoint response.
#[derive(Serialize)]
struct APIHealthResp {
    ok: bool,
}

impl APIServer {
    /// Constructs an API server.
    fn new(cfg: Config) -> APIServer {
        APIServer{
            cfg: cfg,
        }
    }
    
    /// Constructs a Warp filter with all the API endpints setup.
    fn get_routes(&self) -> impl warp::Filter<Extract = impl warp::Reply,Error = warp::Rejection> {
        
    }

    /// Health check endpoint.
    async fn health_endpoint(&self) -> Result<impl warp::Reply, APIError> {
        Ok(pwarp::reply::json(&APIHealthResp{
            ok: true,
        }))
    }
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    // Load configuration
    let cfg = Config::load_from_file("./config.toml")?;

    // Run server
    let api = APIServer::new(cfg);
    let routes = api.get_routes();

    warp::serve(routes).run(([172, 0, 0, 1], cfg.server.port)).await;
    
    Ok(())
}
