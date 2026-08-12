pub mod config;
pub mod model;
pub mod serdes;
pub mod telemetry;

/// Default location of the daemon's YAML configuration file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/agentdesktop/config.yaml";
/// Default location of the controller's YAML configuration file.
pub const DEFAULT_CONTROLLER_CONFIG_PATH: &str = "/etc/agentdesktop/controller.yaml";
/// Default directory for the device identity and other persistent daemon state.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/agentdesktop";

#[cfg(unix)]
/// Default Unix socket exposed by the daemon to local clients.
pub const DEFAULT_SOCKET_PATH: &str = "/run/agentdesktop/agentdesktop.sock";

#[cfg(windows)]
/// Default Windows named pipe exposed by the daemon to local clients.
pub const DEFAULT_SOCKET_PATH: &str = r"\\.\pipe\agentdesktop";
