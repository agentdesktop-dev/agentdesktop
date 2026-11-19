use std::collections::HashSet;

use agentdesktop_core::config::ControllerEnrollmentTokensConfig;
use anyhow::{Context, bail};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::Deserialize;

/// The verified enrollment token a device enrolled with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnrollmentToken {
    pub issuer: String,
    pub id: String,
}

/// Verifies operator-signed enrollment tokens.
#[derive(Clone)]
pub struct EnrollmentTokens {
    audience: String,
    issuers: Vec<Issuer>,
}

#[derive(Clone)]
struct Issuer {
    name: String,
    key: DecodingKey,
    algorithm: Algorithm,
    revoked_ids: HashSet<String>,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    jti: String,
    #[allow(dead_code)]
    iat: u64,
}

impl EnrollmentTokens {
    pub fn load(config: &ControllerEnrollmentTokensConfig) -> anyhow::Result<Self> {
        let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
        let issuers = config
            .issuers
            .iter()
            .map(|issuer| {
                let path = &issuer.public_key_path;
                let pem = std::fs::read(path)
                    .with_context(|| format!("read public key from {}", path.display()))?;
                let (key, algorithm) = public_key(&pem)
                    .with_context(|| format!("parse public key from {}", path.display()))?;
                Ok(Issuer {
                    name: issuer.name.clone(),
                    key,
                    algorithm,
                    revoked_ids: issuer.revoked_ids.iter().cloned().collect(),
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            audience: config.audience.clone(),
            issuers,
        })
    }

    pub fn verify(&self, token: &str) -> anyhow::Result<EnrollmentToken> {
        // Only selects the key; the claims are verified below.
        let issuer = jsonwebtoken::dangerous::insecure_decode_claims::<Claims>(token)
            .context("decode enrollment token")?
            .iss;
        let issuer = self
            .issuers
            .iter()
            .find(|candidate| candidate.name == issuer)
            .with_context(|| format!("enrollment token issuer {issuer} is not configured"))?;
        // The algorithm comes from the configured key, never from the token.
        let mut validation = Validation::new(issuer.algorithm);
        validation.set_issuer(&[&issuer.name]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["iss", "aud"]);
        validation.validate_nbf = true;
        let claims = decode::<Claims>(token, &issuer.key, &validation)
            .context("validate enrollment token")?
            .claims;
        if claims.jti.is_empty() {
            bail!("enrollment token has an empty jti");
        }
        if issuer.revoked_ids.contains(&claims.jti) {
            bail!("enrollment token {} is revoked", claims.jti);
        }
        Ok(EnrollmentToken {
            issuer: claims.iss,
            id: claims.jti,
        })
    }

    /// Whether a device that enrolled with `token` is still accepted.
    pub fn accepts(&self, token: &EnrollmentToken) -> bool {
        self.issuers
            .iter()
            .any(|issuer| issuer.name == token.issuer && !issuer.revoked_ids.contains(&token.id))
    }
}

fn public_key(pem: &[u8]) -> anyhow::Result<(DecodingKey, Algorithm)> {
    DecodingKey::from_ec_pem(pem)
        .map(|key| (key, Algorithm::ES256))
        .or_else(|_| DecodingKey::from_ed_pem(pem).map(|key| (key, Algorithm::EdDSA)))
        .or_else(|_| DecodingKey::from_rsa_pem(pem).map(|key| (key, Algorithm::RS256)))
        .context("expected a PEM-encoded ECDSA P-256, Ed25519 or RSA public key")
}

#[cfg(test)]
pub(crate) mod tests {
    use agentdesktop_core::config::{
        ControllerEnrollmentTokenIssuer, ControllerEnrollmentTokensConfig,
    };
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use serde_json::json;

    use super::{EnrollmentToken, EnrollmentTokens};

    pub(crate) struct Issuer {
        key: EncodingKey,
        directory: tempfile::TempDir,
    }

    impl Issuer {
        pub(crate) fn new() -> Self {
            let key = rcgen::KeyPair::generate().unwrap();
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("jamf.pub.pem"), key.public_key_pem()).unwrap();
            Self {
                key: EncodingKey::from_ec_pem(key.serialize_pem().as_bytes()).unwrap(),
                directory,
            }
        }

        pub(crate) fn tokens(&self, revoked_ids: &[&str]) -> EnrollmentTokens {
            EnrollmentTokens::load(&ControllerEnrollmentTokensConfig {
                audience: "fleet".to_owned(),
                issuers: vec![ControllerEnrollmentTokenIssuer {
                    name: "jamf".to_owned(),
                    public_key_path: self.directory.path().join("jamf.pub.pem"),
                    revoked_ids: revoked_ids.iter().map(|id| (*id).to_owned()).collect(),
                }],
            })
            .unwrap()
        }

        pub(crate) fn sign(&self, claims: serde_json::Value) -> String {
            jsonwebtoken::encode(&Header::new(Algorithm::ES256), &claims, &self.key).unwrap()
        }

        pub(crate) fn token(&self, id: &str) -> String {
            self.sign(json!({"iss": "jamf", "aud": "fleet", "jti": id, "iat": 1}))
        }
    }

    #[test]
    fn verifies_a_token_from_a_configured_issuer() {
        let issuer = Issuer::new();
        let token = issuer.tokens(&[]).verify(&issuer.token("batch-1")).unwrap();
        assert_eq!(
            token,
            EnrollmentToken {
                issuer: "jamf".to_owned(),
                id: "batch-1".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_tokens_it_cannot_trust() {
        let issuer = Issuer::new();
        let tokens = issuer.tokens(&["revoked"]);
        let now = jsonwebtoken::get_current_timestamp();
        let rejected = [
            ("revoked", issuer.token("revoked")),
            ("another key", Issuer::new().token("batch-1")),
            (
                "unknown issuer",
                issuer.sign(json!({"iss": "other", "aud": "fleet", "jti": "a", "iat": 1})),
            ),
            (
                "wrong audience",
                issuer.sign(json!({"iss": "jamf", "aud": "other", "jti": "a", "iat": 1})),
            ),
            (
                "no audience",
                issuer.sign(json!({"iss": "jamf", "jti": "a", "iat": 1})),
            ),
            (
                "no jti",
                issuer.sign(json!({"iss": "jamf", "aud": "fleet", "iat": 1})),
            ),
            (
                "no iat",
                issuer.sign(json!({"iss": "jamf", "aud": "fleet", "jti": "a"})),
            ),
            (
                "expired",
                issuer.sign(json!({
                    "iss": "jamf", "aud": "fleet", "jti": "a", "iat": 1, "exp": now - 3600,
                })),
            ),
            (
                "not yet valid",
                issuer.sign(json!({
                    "iss": "jamf", "aud": "fleet", "jti": "a", "iat": 1, "nbf": now + 3600,
                })),
            ),
        ];
        for (case, token) in rejected {
            assert!(tokens.verify(&token).is_err(), "accepted a token: {case}");
        }
    }

    #[test]
    fn a_revoked_or_removed_token_no_longer_vouches_for_its_devices() {
        let issuer = Issuer::new();
        let token = |issuer: &str, id: &str| EnrollmentToken {
            issuer: issuer.to_owned(),
            id: id.to_owned(),
        };
        let tokens = issuer.tokens(&["revoked"]);
        assert!(tokens.accepts(&token("jamf", "batch-1")));
        assert!(!tokens.accepts(&token("jamf", "revoked")));
        assert!(!tokens.accepts(&token("removed-issuer", "batch-1")));
    }
}
