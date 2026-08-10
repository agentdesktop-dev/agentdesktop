pub mod api;
pub mod client;
pub mod config;
pub mod discovery;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/agentplane/config.yaml";
pub const DEFAULT_SOCKET_PATH: &str = "/run/agentplane/agentplane.sock";
