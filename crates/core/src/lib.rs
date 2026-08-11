pub mod config;
pub mod model;
pub mod serdes;
pub mod telemetry;

pub const DEFAULT_CONFIG_PATH: &str = "/etc/agentdesktop/config.yaml";
pub const DEFAULT_STATE_DIR: &str = "/var/lib/agentdesktop";

#[cfg(unix)]
pub const DEFAULT_SOCKET_PATH: &str = "/run/agentdesktop/agentdesktop.sock";

#[cfg(windows)]
pub const DEFAULT_SOCKET_PATH: &str = r"\\.\pipe\agentdesktop";
