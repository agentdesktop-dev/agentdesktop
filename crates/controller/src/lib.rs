pub mod application;
pub mod daemon_config;
pub mod database;
pub mod device_ca;
pub mod fleet_limits;
pub mod gateway_jwt;
pub mod management;
pub mod oidc;
pub mod service;
pub mod usage;

/// Makes aws-lc-rs rustls's process-wide crypto provider, unless one is
/// already installed. Call it before building any TLS configuration that uses
/// the process default.
///
/// tonic builds the fleet listener's TLS configuration from the process default
/// and has no way to pass a provider. rustls picks the default on its own only
/// when exactly one of its `aws-lc-rs` and `ring` backends is compiled in, so a
/// build that also enables `ring` (a workspace-wide test build does) would
/// otherwise panic when the listener starts.
pub fn install_rustls_provider() {
    // An error only means a provider is already installed.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}
