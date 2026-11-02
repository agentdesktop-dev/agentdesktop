pub mod config;
pub mod http;
pub mod model;
pub mod serdes;
pub mod telemetry;

use std::{env, path::PathBuf};

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
#[cfg(unix)]
pub const DEFAULT_STATE_DIR: &str = "/var/lib/agentdesktop";

/// Default directory for the device identity and other persistent daemon state.
#[cfg(windows)]
pub const DEFAULT_STATE_DIR: &str = r"C:\ProgramData\AgentDesktop\state";

#[cfg(all(unix, not(target_os = "macos")))]
/// Default Unix socket exposed by the daemon to local clients.
pub const DEFAULT_SOCKET_PATH: &str = "/run/agentdesktop/agentdesktop.sock";

#[cfg(target_os = "macos")]
/// Default Unix socket exposed by the daemon to local clients.
pub const DEFAULT_SOCKET_PATH: &str = "/var/run/agentdesktop/agentdesktop.sock";

#[cfg(windows)]
/// Default Windows named pipe exposed by the daemon to local clients.
pub const DEFAULT_SOCKET_PATH: &str = r"\\.\pipe\agentdesktop";

/// Daemon endpoint for local clients: `AGENTDESKTOP_SOCKET` when set, else the
/// system socket when a system daemon is running, else the per-user socket.
pub fn client_socket_path() -> PathBuf {
    env::var_os("AGENTDESKTOP_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let system = PathBuf::from(DEFAULT_SOCKET_PATH);
            if system.exists() {
                return system;
            }
            user_socket_path().unwrap_or(system)
        })
}

#[cfg(unix)]
fn user_socket_path() -> Option<PathBuf> {
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(runtime).join("agentdesktop.sock"));
    }
    let home = env::var_os("HOME").map(PathBuf::from)?;
    let state = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    Some(state.join("agentdesktop/agentdesktop.sock"))
}

#[cfg(windows)]
fn user_socket_path() -> Option<PathBuf> {
    None
}

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
