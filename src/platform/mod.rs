use serde::Serialize;

#[cfg(target_os = "linux")]
pub mod linux;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PlatformCapabilities {
    pub os: &'static str,
    pub native_gateway: bool,
    pub transparent_capture: bool,
    pub trust_installation: bool,
    pub secret_service: bool,
    pub protected_file_credentials: bool,
}

pub const fn capabilities() -> PlatformCapabilities {
    PlatformCapabilities {
        os: std::env::consts::OS,
        native_gateway: true,
        transparent_capture: cfg!(target_os = "linux"),
        trust_installation: cfg!(target_os = "linux"),
        secret_service: cfg!(target_os = "linux"),
        protected_file_credentials: cfg!(target_os = "linux"),
    }
}

#[cfg(test)]
mod tests {
    use super::capabilities;

    #[test]
    fn reports_platform_features() {
        let capabilities = capabilities();

        assert!(capabilities.native_gateway);
        assert_eq!(capabilities.transparent_capture, cfg!(target_os = "linux"));
        assert_eq!(capabilities.trust_installation, cfg!(target_os = "linux"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reports_validated_linux_credential_backends() {
        let capabilities = capabilities();

        assert_eq!(capabilities.os, "linux");
        assert!(capabilities.secret_service);
        assert!(capabilities.protected_file_credentials);
    }
}
