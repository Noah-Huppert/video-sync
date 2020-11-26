use std::fs;
use std::io;
use std::fmt;
use std::convert::From;

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

#[tokio::main]
async fn main() -> Result<(), AppError> {
    // Load configuration
    let cfg = Config::load_from_file("./config.toml")?;

    Ok(())
}
