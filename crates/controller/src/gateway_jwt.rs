use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

#[derive(Clone)]
pub struct GatewayJwtIssuer {
    key: Arc<EncodingKey>,
    issuer: String,
    key_id: String,
    lifetime: Duration,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    sub: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
    act: Actor<'a>,
    client_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    idp: Option<&'a BTreeMap<String, serde_json::Value>>,
}

#[derive(Serialize)]
struct Actor<'a> {
    sub: &'a str,
}

impl GatewayJwtIssuer {
    pub fn from_rsa_pem(
        path: &Path,
        issuer: String,
        key_id: String,
        lifetime: Duration,
    ) -> anyhow::Result<Self> {
        let pem = fs::read(path)
            .with_context(|| format!("read gateway JWT private key from {}", path.display()))?;
        let key = EncodingKey::from_rsa_pem(&pem).context("parse gateway JWT RSA private key")?;
        Ok(Self {
            key: Arc::new(key),
            issuer,
            key_id,
            lifetime,
        })
    }

    pub fn issue(
        &self,
        subject: &str,
        device_id: &str,
        client_id: &str,
        audience: &str,
        idp: Option<&BTreeMap<String, serde_json::Value>>,
    ) -> anyhow::Result<(String, u64)> {
        let issued_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before Unix epoch")?
            .as_secs();
        let expires_at = issued_at.saturating_add(self.lifetime.as_secs());
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.key_id.clone());
        let credential = encode(
            &header,
            &Claims {
                iss: &self.issuer,
                sub: subject,
                aud: audience,
                iat: issued_at,
                exp: expires_at,
                act: Actor { sub: device_id },
                client_id,
                email: idp
                    .and_then(|claims| claims.get("email"))
                    .and_then(serde_json::Value::as_str),
                email_verified: idp
                    .and_then(|claims| claims.get("email_verified"))
                    .and_then(serde_json::Value::as_bool),
                idp,
            },
            &self.key,
        )
        .context("sign gateway JWT")?;
        Ok((credential, expires_at))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::{Actor, Claims};

    #[test]
    fn gateway_claims_include_actor_client_and_idp_identity() {
        let idp = BTreeMap::from([
            ("iss".to_owned(), json!("https://idp.example.com")),
            ("sub".to_owned(), json!("user-123")),
            ("email".to_owned(), json!("john@example.com")),
            ("email_verified".to_owned(), json!(true)),
            ("groups".to_owned(), json!(["engineering"])),
        ]);
        let claims = serde_json::to_value(Claims {
            iss: "agentdesktop-controller",
            sub: "user-123",
            aud: "agentgateway",
            iat: 100,
            exp: 200,
            act: Actor { sub: "device-123" },
            client_id: "codex",
            email: Some("john@example.com"),
            email_verified: Some(true),
            idp: Some(&idp),
        })
        .expect("serialize claims");

        assert_eq!(claims["sub"], "user-123");
        assert_eq!(claims["email"], "john@example.com");
        assert_eq!(claims["email_verified"], true);
        assert_eq!(claims["act"]["sub"], "device-123");
        assert_eq!(claims["client_id"], "codex");
        assert_eq!(claims["idp"]["groups"][0], "engineering");
        assert!(claims.get("device_id").is_none());
    }
}
