use std::net::SocketAddr;

use anyhow::{Result, bail};
use clap::Parser;
use url::Url;

#[derive(Clone, Debug, Parser)]
#[command(version, about)]
pub struct Config {
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

        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::Config;
    use clap::Parser;

    fn parse(args: &[&str]) -> anyhow::Result<Config> {
        Config::try_parse_from(args)?.validate()
    }

    #[test]
    fn accepts_loopback_listener_and_http_upstream() {
        let config = parse(&[
            "connector",
            "--listen",
            "[::1]:9000",
            "--upstream",
            "https://gateway.example/base/",
        ])
        .unwrap();

        assert_eq!(config.listen.to_string(), "[::1]:9000");
        assert_eq!(config.upstream.as_str(), "https://gateway.example/base/");
    }

    #[test]
    fn rejects_non_loopback_listener() {
        let error = parse(&[
            "connector",
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
            let error = parse(&["connector", "--upstream", upstream]).unwrap_err();
            assert!(error.to_string().contains("query string or fragment"));
        }
    }
}
