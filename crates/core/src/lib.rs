pub mod config;
pub mod model;
pub mod serdes;
pub mod telemetry;

/// Version reported by endpoint processes. Release builds can override the
/// workspace package version without rewriting Cargo manifests.
pub const VERSION: &str = match option_env!("AGENTDESKTOP_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

/// Default location of the daemon's YAML configuration file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/agentdesktop/config.yaml";
/// Default location of the controller's YAML configuration file.
pub const DEFAULT_CONTROLLER_CONFIG_PATH: &str = "/etc/agentdesktop/controller.yaml";
/// Default directory for the device identity and other persistent daemon state.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/agentdesktop";

#[cfg(all(unix, not(target_os = "macos")))]
/// Default Unix socket exposed by the daemon to local clients.
pub const DEFAULT_SOCKET_PATH: &str = "/run/agentdesktop/agentdesktop.sock";

#[cfg(target_os = "macos")]
/// Default Unix socket exposed by the daemon to local clients.
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/agentdesktop/agentdesktop.sock";

#[cfg(windows)]
/// Default Windows named pipe exposed by the daemon to local clients.
pub const DEFAULT_SOCKET_PATH: &str = r"\\.\pipe\agentdesktop";

#[cfg(test)]
mod tests {
    use super::DEFAULT_SOCKET_PATH;

    #[test]
    fn default_socket_uses_platform_runtime_directory() {
        #[cfg(target_os = "macos")]
        assert_eq!(
            DEFAULT_SOCKET_PATH,
            "/var/run/agentdesktop/agentdesktop.sock"
        );

        #[cfg(all(unix, not(target_os = "macos")))]
        assert_eq!(DEFAULT_SOCKET_PATH, "/run/agentdesktop/agentdesktop.sock");

        #[cfg(windows)]
        assert_eq!(DEFAULT_SOCKET_PATH, r"\\.\pipe\agentdesktop");
    }
}
