use std::{
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
    device_id: &'a str,
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
        audience: &str,
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
                device_id,
            },
            &self.key,
        )
        .context("sign gateway JWT")?;
        Ok((credential, expires_at))
    }
}
