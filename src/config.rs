use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use url::Url;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum DeploymentMode {
    Standalone,
    Managed,
}

#[derive(Clone, Debug, Parser)]
#[command(version, about)]
pub struct Config {
    /// Connector deployment mode.
    #[arg(long, env = "AGENTGATEWAY_EDGE_MODE")]
    pub mode: DeploymentMode,

    /// Loopback address on which to accept Claude traffic.
    #[arg(
        long,
        env = "AGENTGATEWAY_EDGE_LISTEN",
        default_value = "127.0.0.1:8080"
    )]
    pub listen: SocketAddr,

    /// Base URL of the Agent Gateway upstream.
    #[arg(long, env = "AGENTGATEWAY_EDGE_UPSTREAM")]
    pub upstream: Url,

    /// Agent Gateway executable to manage in standalone mode.
    #[arg(long, env = "AGENTGATEWAY_EDGE_GATEWAY_BINARY")]
    pub gateway_binary: Option<PathBuf>,

    /// Agent Gateway configuration passed to the managed executable.
    #[arg(long, env = "AGENTGATEWAY_EDGE_GATEWAY_CONFIG")]
    pub gateway_config: Option<PathBuf>,
}

impl Config {
    pub fn parse_and_validate() -> Result<Self> {
        Self::parse().validate()
    }

    fn validate(self) -> Result<Self> {
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

        match (&self.gateway_binary, &self.gateway_config) {
            (Some(_), Some(_)) if self.mode == DeploymentMode::Managed => {
                bail!("local Agent Gateway lifecycle is only available in standalone mode");
            }
            (Some(_), None) | (None, Some(_)) => {
                bail!("gateway binary and config must be provided together");
            }
            _ => {}
        }

        Ok(self)
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
    use super::{Config, DeploymentMode};
    use clap::Parser;

    fn parse(args: &[&str]) -> anyhow::Result<Config> {
        Config::try_parse_from(args)?.validate()
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
}
