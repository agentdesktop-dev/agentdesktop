pub mod config;
pub mod model;
pub mod serdes;
pub mod telemetry;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/agentplane/config.yaml";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/agentplane";

#[cfg(unix)]
pub const DEFAULT_SOCKET_PATH: &str = "/run/agentplane/agentplane.sock";

#[cfg(windows)]
pub const DEFAULT_SOCKET_PATH: &str = r"\\.\pipe\agentplane";
