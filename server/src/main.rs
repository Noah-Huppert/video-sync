use std::fs;

use serde_derive::Deserialize;
use toml;
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

impl Config {
    /// Load configuration from a TOML file.
    fn load_from_file(path: &str) -> Result<Config, String> {
        let contents = fs::read_to_string(path)?;
        toml::from_str(contents)
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    // Load configuration
    let cfg = Config.load_from_file("./config.toml")?;
}
