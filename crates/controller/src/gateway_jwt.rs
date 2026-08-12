use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use aws_lc_rs::{rsa::KeyPair as RsaKeyPair, signature::KeyPair as _};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

#[derive(Clone)]
pub struct GatewayJwtIssuer {
    key: Arc<EncodingKey>,
    jwks: GatewayJwks,
    issuer: String,
    key_id: String,
    lifetime: Duration,
}

/// Public JSON Web Key Set corresponding to the controller's signing key.
#[derive(Clone, Debug, Serialize)]
pub struct GatewayJwks {
    keys: Vec<GatewayJwk>,
}

#[derive(Clone, Debug, Serialize)]
struct GatewayJwk {
    kty: &'static str,
    #[serde(rename = "use")]
    key_use: &'static str,
    alg: &'static str,
    kid: String,
    n: String,
    e: String,
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
        let jwks = jwks_from_rsa_pem(&pem, &key_id)?;
        Ok(Self {
            key: Arc::new(key),
            jwks,
            issuer,
            key_id,
            lifetime,
        })
    }

    /// Returns the public key set used to verify credentials issued by this controller.
    pub fn jwks(&self) -> GatewayJwks {
        self.jwks.clone()
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

fn jwks_from_rsa_pem(pem: &[u8], key_id: &str) -> anyhow::Result<GatewayJwks> {
    let (label, der) =
        pem_rfc7468::decode_vec(pem).context("decode gateway JWT private key PEM")?;
    let key_pair = match label {
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        _ => anyhow::bail!("gateway JWT private key must be PKCS#8 or PKCS#1 RSA PEM"),
    }
    .map_err(|_| anyhow::anyhow!("parse gateway JWT RSA private key components"))?;
    let public_key = key_pair.public_key();
    Ok(GatewayJwks {
        keys: vec![GatewayJwk {
            kty: "RSA",
            key_use: "sig",
            alg: "RS256",
            kid: key_id.to_owned(),
            n: URL_SAFE_NO_PAD.encode(public_key.modulus().big_endian_without_leading_zero()),
            e: URL_SAFE_NO_PAD.encode(public_key.exponent().big_endian_without_leading_zero()),
        }],
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use aws_lc_rs::{
        encoding::{AsDer, Pkcs8V1Der},
        rsa::{KeyPair, KeySize},
    };
    use jsonwebtoken::{DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
    use pem_rfc7468::LineEnding;
    use serde_json::json;

    use super::{Actor, Claims, GatewayJwtIssuer};

    #[test]
    fn published_jwks_verifies_issued_credentials() {
        let key_pair = KeyPair::generate(KeySize::Rsa2048).expect("generate RSA key");
        let der: Pkcs8V1Der<'static> = key_pair.as_der().expect("encode private key");
        let pem = pem_rfc7468::encode_string("PRIVATE KEY", LineEnding::LF, der.as_ref())
            .expect("encode private key PEM");
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-jwks-test-{}-{}.pem",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::write(&path, pem).expect("write test private key");

        let issuer = GatewayJwtIssuer::from_rsa_pem(
            &path,
            "agentdesktop-controller".to_owned(),
            "agentdesktop".to_owned(),
            std::time::Duration::from_secs(300),
        )
        .expect("create issuer");
        let (credential, _) = issuer
            .issue("user", "device", "codex", "agentgateway", None)
            .expect("issue credential");
        let header = decode_header(&credential).expect("decode credential header");
        assert_eq!(header.kid.as_deref(), Some("agentdesktop"));

        let jwks: JwkSet =
            serde_json::from_value(serde_json::to_value(issuer.jwks()).expect("serialize JWKS"))
                .expect("decode JWKS");
        let key = jwks.find("agentdesktop").expect("find signing key");
        let decoding_key = DecodingKey::from_jwk(key).expect("construct decoding key");
        let mut validation = Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_issuer(&["agentdesktop-controller"]);
        validation.set_audience(&["agentgateway"]);
        decode::<serde_json::Value>(&credential, &decoding_key, &validation)
            .expect("verify credential with published JWKS");

        let _ = fs::remove_file(path);
    }

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
