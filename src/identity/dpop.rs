use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::Generate;
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
pub struct PublicJwk {
    pub kty: &'static str,
    pub crv: &'static str,
    pub x: String,
    pub y: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Es256VerificationJwk {
    pub kid: String,
    pub kty: String,
    pub crv: String,
    pub x: String,
    pub y: String,
}

pub struct DpopKey {
    signing_key: SigningKey,
}

impl DpopKey {
    pub fn generate() -> Self {
        Self {
            signing_key: SigningKey::generate(),
        }
    }

    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self> {
        Ok(Self {
            signing_key: SigningKey::from_pkcs8_der(der)?,
        })
    }

    pub fn to_pkcs8_der(&self) -> Result<Vec<u8>> {
        Ok(self.signing_key.to_pkcs8_der()?.as_bytes().to_vec())
    }

    pub fn public_jwk(&self) -> PublicJwk {
        let point = self.signing_key.verifying_key().to_sec1_point(false);
        PublicJwk {
            kty: "EC",
            crv: "P-256",
            x: encode(point.x().expect("uncompressed P-256 point has x")),
            y: encode(point.y().expect("uncompressed P-256 point has y")),
        }
    }

    pub fn thumbprint(&self) -> Result<String> {
        let jwk = self.public_jwk();
        let canonical = serde_json::to_vec(&serde_json::json!({
            "crv": jwk.crv,
            "kty": jwk.kty,
            "x": jwk.x,
            "y": jwk.y,
        }))?;
        Ok(encode(Sha256::digest(canonical)))
    }

    pub fn proof(
        &self,
        method: &str,
        target_uri: &str,
        access_token: Option<&str>,
    ) -> Result<String> {
        let mut identifier = [0_u8; 32];
        getrandom::fill(&mut identifier)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut claims = serde_json::json!({
            "htm": method,
            "htu": target_uri,
            "iat": now,
            "jti": encode(identifier),
        });
        if let Some(access_token) = access_token {
            claims["ath"] = serde_json::Value::String(encode(Sha256::digest(access_token)));
        }
        self.sign_jwt(
            &serde_json::json!({
                "typ": "dpop+jwt",
                "alg": "ES256",
                "jwk": self.public_jwk(),
            }),
            &claims,
        )
    }

    fn sign_jwt(&self, header: &serde_json::Value, claims: &serde_json::Value) -> Result<String> {
        let input = format!(
            "{}.{}",
            encode(serde_json::to_vec(header)?),
            encode(serde_json::to_vec(claims)?)
        );
        let signature: Signature = self.signing_key.sign(input.as_bytes());
        Ok(format!("{input}.{}", encode(signature.to_bytes())))
    }
}

pub fn decode_jwt_claims(token: &str) -> Result<serde_json::Value> {
    let payload = token
        .split('.')
        .nth(1)
        .context("access token is not a compact JWT")?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?;
    Ok(serde_json::from_slice(&decoded)?)
}

pub fn verify_es256_jwt(token: &str, keys: &[Es256VerificationJwk]) -> Result<serde_json::Value> {
    let segments: Vec<_> = token.split('.').collect();
    if segments.len() != 3 {
        bail!("access token is not a compact JWT");
    }
    let header: serde_json::Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(segments[0])?,
    )?;
    if header["alg"] != "ES256" {
        bail!("access token does not use ES256");
    }
    let key_id = header["kid"]
        .as_str()
        .context("access token header has no key identifier")?;
    let key = keys
        .iter()
        .find(|key| key.kid == key_id)
        .context("access token signing key is absent from issuer JWKS")?;
    if key.kty != "EC" || key.crv != "P-256" {
        bail!("access token signing key is not an EC P-256 key");
    }
    let x = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&key.x)?;
    let y = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&key.y)?;
    if x.len() != 32 || y.len() != 32 {
        bail!("access token signing key has invalid P-256 coordinates");
    }
    let mut encoded_point = Vec::with_capacity(65);
    encoded_point.push(4);
    encoded_point.extend_from_slice(&x);
    encoded_point.extend_from_slice(&y);
    let verifying_key = VerifyingKey::from_sec1_bytes(&encoded_point)?;
    let signature_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(segments[2])?;
    let signature = Signature::from_slice(&signature_bytes)?;
    verifying_key.verify(
        format!("{}.{}", segments[0], segments[1]).as_bytes(),
        &signature,
    )?;
    decode_jwt_claims(token)
}

fn encode(value: impl AsRef<[u8]>) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value)
}

#[cfg(test)]
mod tests {
    use super::{DpopKey, Es256VerificationJwk, decode_jwt_claims, verify_es256_jwt};

    #[test]
    fn key_round_trip_preserves_thumbprint() {
        let key = DpopKey::generate();
        let loaded = DpopKey::from_pkcs8_der(&key.to_pkcs8_der().unwrap()).unwrap();
        assert_eq!(loaded.thumbprint().unwrap(), key.thumbprint().unwrap());
    }

    #[test]
    fn request_proof_contains_token_hash() {
        let proof = DpopKey::generate()
            .proof("POST", "https://gateway.example/v1/messages", Some("token"))
            .unwrap();
        let claims = decode_jwt_claims(&proof).unwrap();
        assert_eq!(claims["htm"], "POST");
        assert_eq!(claims["htu"], "https://gateway.example/v1/messages");
        assert!(claims["ath"].is_string());
    }

    #[test]
    fn jwt_verification_rejects_tampering() {
        let key = DpopKey::generate();
        let token = key
            .sign_jwt(
                &serde_json::json!({"alg": "ES256", "kid": "key-1"}),
                &serde_json::json!({"sub": "user-1"}),
            )
            .unwrap();
        let public = key.public_jwk();
        let keys = [Es256VerificationJwk {
            kid: "key-1".into(),
            kty: public.kty.into(),
            crv: public.crv.into(),
            x: public.x,
            y: public.y,
        }];
        assert_eq!(verify_es256_jwt(&token, &keys).unwrap()["sub"], "user-1");

        let mut segments: Vec<_> = token.split('.').collect();
        segments[1] = "eyJzdWIiOiJhdHRhY2tlciJ9";
        assert!(verify_es256_jwt(&segments.join("."), &keys).is_err());
    }
}
