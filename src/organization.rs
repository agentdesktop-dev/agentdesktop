use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationBootstrap {
    pub format_version: u32,
    pub organization: Organization,
    pub identity: IdentityBootstrap,
    pub gateway: GatewayBootstrap,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Organization {
    pub id: String,
    pub display_name: String,
    pub support_url: Url,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityBootstrap {
    pub issuer: Url,
    pub client_id: String,
    pub audience: String,
    pub scope: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayBootstrap {
    pub url: Url,
}

impl OrganizationBootstrap {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let bootstrap: Self =
            serde_json::from_slice(bytes).context("organization bootstrap is not valid JSON")?;
        bootstrap.validate()?;
        Ok(bootstrap)
    }

    pub fn validate(&self) -> Result<()> {
        if self.format_version != 1 {
            bail!("unsupported organization bootstrap format");
        }
        validate_identifier("organization ID", &self.organization.id)?;
        validate_text("organization display name", &self.organization.display_name)?;
        validate_https_url(
            "organization support URL",
            &self.organization.support_url,
            true,
        )?;
        validate_https_url("identity issuer", &self.identity.issuer, false)?;
        validate_text("OAuth client ID", &self.identity.client_id)?;
        validate_text("OAuth audience", &self.identity.audience)?;
        validate_scope(&self.identity.scope)?;
        validate_https_url("Agent Gateway URL", &self.gateway.url, false)?;
        if self.gateway.url.path() != "/" {
            bail!("Agent Gateway URL must be an origin without a path");
        }
        Ok(())
    }
}

fn validate_identifier(name: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("{name} must contain only ASCII letters, numbers, '-' or '_'");
    }
    Ok(())
}

fn validate_text(name: &str, value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("{name} must be non-empty text without control characters");
    }
    Ok(())
}

fn validate_scope(scope: &str) -> Result<()> {
    validate_text("OAuth scope", scope)?;
    let items: Vec<_> = scope.split_ascii_whitespace().collect();
    if items.is_empty() || items.iter().any(|item| !item.is_ascii()) || items.join(" ") != scope {
        bail!("OAuth scope must contain space-separated ASCII values");
    }
    Ok(())
}

fn validate_https_url(name: &str, url: &Url, allow_path: bool) -> Result<()> {
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (!allow_path && url.path() != "/")
    {
        bail!("{name} must be an HTTPS URL without credentials, query, or fragment");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::OrganizationBootstrap;

    fn valid() -> Vec<u8> {
        br#"{
          "format_version": 1,
          "organization": {
            "id": "acme",
            "display_name": "Acme Corporation",
            "support_url": "https://help.acme.example/agentdesktop"
          },
          "identity": {
            "issuer": "https://login.acme.example/",
            "client_id": "agentdesktop",
            "audience": "https://gateway.acme.example",
            "scope": "agentgateway.invoke"
          },
          "gateway": { "url": "https://gateway.acme.example/" }
        }"#
        .to_vec()
    }

    #[test]
    fn accepts_a_strict_non_secret_bootstrap() {
        let bootstrap = OrganizationBootstrap::parse(&valid()).unwrap();
        assert_eq!(bootstrap.organization.id, "acme");
        assert_eq!(
            bootstrap.gateway.url.as_str(),
            "https://gateway.acme.example/"
        );
    }

    #[test]
    fn rejects_unknown_or_insecure_configuration() {
        let mut insecure: serde_json::Value = serde_json::from_slice(&valid()).unwrap();
        insecure["gateway"]["url"] = "http://gateway.acme.example".into();
        assert!(OrganizationBootstrap::parse(&serde_json::to_vec(&insecure).unwrap()).is_err());

        let mut secret: serde_json::Value = serde_json::from_slice(&valid()).unwrap();
        secret["identity"]["client_secret"] = "must-not-be-embedded".into();
        assert!(OrganizationBootstrap::parse(&serde_json::to_vec(&secret).unwrap()).is_err());
    }
}
