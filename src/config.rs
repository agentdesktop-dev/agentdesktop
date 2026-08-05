use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Args, ValueEnum};
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum DeploymentMode {
    Standalone,
    Managed,
}

#[derive(Args, Clone, Debug)]
pub struct Config {
    /// Connector deployment mode.
    #[arg(long, env = "AGENTDESKTOP_MODE")]
    pub mode: DeploymentMode,

    /// Loopback address on which to accept Claude traffic.
    #[arg(long, env = "AGENTDESKTOP_LISTEN", default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,

    /// Base URL of the Agent Gateway upstream.
    #[arg(long, env = "AGENTDESKTOP_UPSTREAM")]
    pub upstream: Url,

    /// Agent Gateway executable to manage in standalone mode.
    #[arg(long, env = "AGENTDESKTOP_GATEWAY_BINARY")]
    pub gateway_binary: Option<PathBuf>,

    /// Agent Gateway configuration passed to the managed executable.
    #[arg(long, env = "AGENTDESKTOP_GATEWAY_CONFIG")]
    pub gateway_config: Option<PathBuf>,

    /// Authorization-server issuer for DPoP-authenticated managed forwarding.
    #[arg(long, env = "AGENTDESKTOP_IDENTITY_ISSUER")]
    pub identity_issuer: Option<Url>,

    /// Enrollment service origin for managed device certificate renewal.
    #[arg(long, env = "AGENTDESKTOP_ENROLLMENT_URL")]
    pub enrollment_url: Option<Url>,

    /// Directory containing the persisted managed identity backend selection.
    #[arg(long, env = "AGENTDESKTOP_IDENTITY_DIR")]
    pub identity_dir: Option<PathBuf>,

    /// Maximum time to establish an upstream connection.
    #[arg(long, env = "AGENTDESKTOP_CONNECT_TIMEOUT_MS", default_value_t = 5_000)]
    pub connect_timeout_ms: u64,

    /// Maximum time to receive upstream response headers.
    #[arg(
        long,
        env = "AGENTDESKTOP_REQUEST_TIMEOUT_MS",
        default_value_t = 30_000
    )]
    pub request_timeout_ms: u64,

    /// Maximum time to drain requests after shutdown begins.
    #[arg(
        long,
        env = "AGENTDESKTOP_SHUTDOWN_TIMEOUT_MS",
        default_value_t = 10_000
    )]
    pub shutdown_timeout_ms: u64,

    /// Maximum number of requests forwarding or streaming concurrently.
    #[arg(long, env = "AGENTDESKTOP_MAX_IN_FLIGHT", default_value_t = 128)]
    pub max_in_flight: usize,

    /// Enable the standalone Linux transparent-capture relay.
    #[cfg(target_os = "linux")]
    #[arg(long, env = "AGENTDESKTOP_CAPTURE_ENABLED", default_value_t = false)]
    pub capture_enabled: bool,
}

impl Config {
    pub fn validate(self) -> Result<Self> {
        if !self.listen.ip().is_loopback() {
            bail!("listen address must be loopback, got {}", self.listen);
        }

        if !matches!(self.upstream.scheme(), "http" | "https") {
            bail!(
                "upstream URL must use http or https, got {}",
                self.upstream.scheme()
            );
        }

        if self.upstream.cannot_be_a_base() || self.upstream.host_str().is_none() {
            bail!("upstream URL must be an absolute base URL");
        }

        if self.upstream.query().is_some() || self.upstream.fragment().is_some() {
            bail!("upstream URL must not contain a query string or fragment");
        }

        if self.mode == DeploymentMode::Standalone && !is_local_host(&self.upstream) {
            bail!(
                "standalone mode requires a loopback Agent Gateway upstream, got {}",
                self.upstream
            );
        }

        if self.mode == DeploymentMode::Standalone && self.identity_issuer.is_some() {
            bail!("managed identity is only available in managed mode");
        }
        if self.mode == DeploymentMode::Standalone && self.enrollment_url.is_some() {
            bail!("certificate enrollment is only available in managed mode");
        }
        if self.identity_issuer.is_some() != self.enrollment_url.is_some() {
            bail!("managed identity issuer and enrollment URL must be provided together");
        }
        if let Some(enrollment_url) = &self.enrollment_url
            && (enrollment_url.scheme() != "https"
                || enrollment_url.host_str().is_none()
                || enrollment_url.path() != "/"
                || enrollment_url.query().is_some()
                || enrollment_url.fragment().is_some())
        {
            bail!("enrollment URL must be an HTTPS origin");
        }
        if self.identity_dir.is_some() && self.identity_issuer.is_none() {
            bail!("identity directory requires an identity issuer");
        }
        if self.connect_timeout_ms == 0
            || self.request_timeout_ms == 0
            || self.shutdown_timeout_ms == 0
            || self.max_in_flight == 0
        {
            bail!("timeouts and max in-flight requests must be greater than zero");
        }

        match (&self.gateway_binary, &self.gateway_config) {
            (Some(_), Some(_)) if self.mode == DeploymentMode::Managed => {
                bail!("local Agent Gateway lifecycle is only available in standalone mode");
            }
            (Some(_), None) | (None, Some(_)) => {
                bail!("gateway binary and config must be provided together");
            }
            _ => {}
        }

        #[cfg(target_os = "linux")]
        if self.capture_enabled
            && (self.mode != DeploymentMode::Standalone || self.gateway_binary.is_none())
        {
            bail!("transparent capture requires an owned standalone Agent Gateway");
        }

        Ok(self)
    }
}

pub fn upstream_origin(upstream: &Url) -> Result<Url> {
    Ok(Url::parse(&upstream.origin().ascii_serialization())?)
}

impl DeploymentMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Standalone => "standalone",
            Self::Managed => "managed",
        }
    }
}

fn is_local_host(upstream: &Url) -> bool {
    upstream.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

#[cfg(test)]
mod tests {
    use super::{Config, DeploymentMode, upstream_origin};
    use clap::Parser;
    use url::Url;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        config: Config,
    }

    fn parse(args: &[&str]) -> anyhow::Result<Config> {
        Cli::try_parse_from(args)?.config.validate()
    }

    #[test]
    fn accepts_loopback_listener_and_http_upstream() {
        let config = parse(&[
            "connector",
            "--mode",
            "managed",
            "--listen",
            "[::1]:9000",
            "--upstream",
            "https://gateway.example/base/",
        ])
        .unwrap();

        assert_eq!(config.listen.to_string(), "[::1]:9000");
        assert_eq!(config.upstream.as_str(), "https://gateway.example/base/");
        assert_eq!(config.mode, DeploymentMode::Managed);
    }

    #[test]
    fn accepts_loopback_upstream_in_standalone_mode() {
        for upstream in ["http://localhost:4000", "http://127.0.0.1:4000"] {
            let config =
                parse(&["connector", "--mode", "standalone", "--upstream", upstream]).unwrap();

            assert_eq!(config.mode, DeploymentMode::Standalone);
        }
    }

    #[test]
    fn rejects_remote_upstream_in_standalone_mode() {
        let error = parse(&[
            "connector",
            "--mode",
            "standalone",
            "--upstream",
            "https://gateway.example",
        ])
        .unwrap_err();

        assert!(error.to_string().contains("requires a loopback"));
    }

    #[test]
    fn rejects_non_loopback_listener() {
        let error = parse(&[
            "connector",
            "--mode",
            "managed",
            "--listen",
            "0.0.0.0:8080",
            "--upstream",
            "http://gateway.example",
        ])
        .unwrap_err();

        assert!(error.to_string().contains("must be loopback"));
    }

    #[test]
    fn rejects_unsupported_upstream_scheme() {
        let error = parse(&[
            "connector",
            "--mode",
            "managed",
            "--upstream",
            "file:///var/run/agentgateway.sock",
        ])
        .unwrap_err();

        assert!(error.to_string().contains("must use http or https"));
    }

    #[test]
    fn rejects_upstream_query_or_fragment() {
        for upstream in [
            "https://gateway.example/?tenant=one",
            "https://gateway.example/#fragment",
        ] {
            let error =
                parse(&["connector", "--mode", "managed", "--upstream", upstream]).unwrap_err();
            assert!(error.to_string().contains("query string or fragment"));
        }
    }

    #[test]
    fn requires_deployment_mode() {
        let error = parse(&["connector", "--upstream", "http://127.0.0.1:4000"]).unwrap_err();

        assert!(error.to_string().contains("--mode"));
    }

    #[test]
    fn accepts_local_gateway_lifecycle_in_standalone_mode() {
        let config = parse(&[
            "connector",
            "--mode",
            "standalone",
            "--upstream",
            "http://127.0.0.1:4000",
            "--gateway-binary",
            "/usr/bin/agentgateway",
            "--gateway-config",
            "/etc/agentgateway/config.yaml",
        ])
        .unwrap();

        assert_eq!(
            config.gateway_binary.unwrap(),
            std::path::Path::new("/usr/bin/agentgateway")
        );
    }

    #[test]
    fn rejects_partial_local_gateway_lifecycle_configuration() {
        let error = parse(&[
            "connector",
            "--mode",
            "standalone",
            "--upstream",
            "http://127.0.0.1:4000",
            "--gateway-binary",
            "/usr/bin/agentgateway",
        ])
        .unwrap_err();

        assert!(error.to_string().contains("must be provided together"));
    }

    #[test]
    fn rejects_local_gateway_lifecycle_in_managed_mode() {
        let error = parse(&[
            "connector",
            "--mode",
            "managed",
            "--upstream",
            "https://gateway.example",
            "--gateway-binary",
            "/usr/bin/agentgateway",
            "--gateway-config",
            "/etc/agentgateway/config.yaml",
        ])
        .unwrap_err();

        assert!(error.to_string().contains("only available in standalone"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn capture_requires_owned_standalone_gateway() {
        let external = parse(&[
            "connector",
            "--mode",
            "standalone",
            "--upstream",
            "http://127.0.0.1:4000",
            "--capture-enabled",
        ])
        .unwrap_err();
        assert!(external.to_string().contains("owned standalone"));

        let managed = parse(&[
            "connector",
            "--mode",
            "managed",
            "--upstream",
            "https://gateway.example",
            "--capture-enabled",
        ])
        .unwrap_err();
        assert!(managed.to_string().contains("owned standalone"));
    }

    #[test]
    fn derives_identity_origin_without_upstream_path() {
        let origin =
            upstream_origin(&Url::parse("https://gateway.example:8443/base/path/").unwrap())
                .unwrap();

        assert_eq!(origin.as_str(), "https://gateway.example:8443/");
    }

    #[test]
    fn accepts_managed_identity_configuration() {
        let config = parse(&[
            "connector",
            "--mode",
            "managed",
            "--upstream",
            "https://gateway.example/base/",
            "--identity-issuer",
            "https://identity.example/",
            "--enrollment-url",
            "https://enrollment.example/",
        ])
        .unwrap();

        assert_eq!(
            config.identity_issuer.unwrap().as_str(),
            "https://identity.example/"
        );
        assert_eq!(
            config.enrollment_url.unwrap().as_str(),
            "https://enrollment.example/"
        );
    }

    #[test]
    fn rejects_partial_or_insecure_enrollment_configuration() {
        let missing = parse(&[
            "connector",
            "--mode",
            "managed",
            "--upstream",
            "https://gateway.example/",
            "--identity-issuer",
            "https://identity.example/",
        ])
        .unwrap_err();
        assert!(missing.to_string().contains("provided together"));

        let insecure = parse(&[
            "connector",
            "--mode",
            "managed",
            "--upstream",
            "https://gateway.example/",
            "--identity-issuer",
            "https://identity.example/",
            "--enrollment-url",
            "http://enrollment.example/",
        ])
        .unwrap_err();
        assert!(insecure.to_string().contains("HTTPS origin"));
    }

    #[test]
    fn rejects_zero_resource_limits() {
        for argument in [
            "--connect-timeout-ms",
            "--request-timeout-ms",
            "--shutdown-timeout-ms",
            "--max-in-flight",
        ] {
            let error = parse(&[
                "connector",
                "--mode",
                "standalone",
                "--upstream",
                "http://127.0.0.1:4000",
                argument,
                "0",
            ])
            .unwrap_err();
            assert!(error.to_string().contains("greater than zero"));
        }
    }
}
