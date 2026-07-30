use anyhow::{Result, bail};
use clap::ValueEnum;
use url::Url;

const NATIVE_BASE_URL: &str = "http://127.0.0.1:4000";
const CONNECTOR_BASE_URL: &str = "http://127.0.0.1:8080";

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum ClaudePath {
    Native,
    Connector,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeConfig {
    pub base_url: Url,
    pub api_key: String,
}

impl ClaudeConfig {
    pub fn standalone(path: ClaudePath, base_url: Option<Url>, api_key: String) -> Result<Self> {
        let base_url = match base_url {
            Some(base_url) => base_url,
            None => Url::parse(match path {
                ClaudePath::Native => NATIVE_BASE_URL,
                ClaudePath::Connector => CONNECTOR_BASE_URL,
            })?,
        };

        if !matches!(base_url.scheme(), "http" | "https")
            || base_url.cannot_be_a_base()
            || base_url.host_str().is_none()
        {
            bail!("Claude base URL must be an absolute HTTP or HTTPS URL");
        }
        if base_url.query().is_some() || base_url.fragment().is_some() {
            bail!("Claude base URL must not contain a query string or fragment");
        }
        if !base_url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }) {
            bail!("standalone Claude path requires a loopback base URL");
        }
        if api_key.is_empty() {
            bail!("Claude placeholder credential must not be empty");
        }

        Ok(Self { base_url, api_key })
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaudeConfig, ClaudePath};
    use url::Url;

    #[test]
    fn selects_path_defaults() {
        let native =
            ClaudeConfig::standalone(ClaudePath::Native, None, "placeholder".to_owned()).unwrap();
        let connector =
            ClaudeConfig::standalone(ClaudePath::Connector, None, "placeholder".to_owned())
                .unwrap();

        assert_eq!(native.base_url.as_str(), "http://127.0.0.1:4000/");
        assert_eq!(connector.base_url.as_str(), "http://127.0.0.1:8080/");
    }

    #[test]
    fn accepts_custom_loopback_url() {
        let config = ClaudeConfig::standalone(
            ClaudePath::Native,
            Some(Url::parse("https://localhost:4443/gateway/").unwrap()),
            "placeholder".to_owned(),
        )
        .unwrap();

        assert_eq!(config.base_url.as_str(), "https://localhost:4443/gateway/");
    }

    #[test]
    fn rejects_remote_url() {
        let error = ClaudeConfig::standalone(
            ClaudePath::Connector,
            Some(Url::parse("https://gateway.example").unwrap()),
            "placeholder".to_owned(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("requires a loopback"));
    }
}
