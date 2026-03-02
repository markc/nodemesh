use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub node: NodeConfig,
    pub bridge: BridgeConfig,
    #[serde(default)]
    pub peers: HashMap<String, PeerConfig>,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Deserialize)]
pub struct NodeConfig {
    pub name: String,
    pub wg_ip: String,
    #[serde(default = "default_listen")]
    pub listen: String,
}

#[derive(Debug, Deserialize)]
pub struct BridgeConfig {
    #[serde(default = "default_socket")]
    pub socket: String,
    pub callback_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PeerConfig {
    pub wg_ip: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

fn default_listen() -> String {
    "0.0.0.0:9800".to_string()
}

fn default_socket() -> String {
    "/run/meshd/meshd.sock".to_string()
}

fn default_port() -> u16 {
    9800
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}
