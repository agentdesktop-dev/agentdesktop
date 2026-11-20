use std::collections::HashSet;

use agentdesktop_core::config::ControllerEnrollmentTokensConfig;
use anyhow::{Context, bail};
use jsonwebtoken::{DecodingKey, Validation, decode};
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
    revoked_ids: HashSet<String>,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    jti: String,
    /// Required, so that a token records when it was issued. Not checked.
    #[serde(rename = "iat")]
    _issued_at: u64,
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
                let key = public_key(&pem)
                    .with_context(|| format!("parse public key from {}", path.display()))?;
                Ok(Issuer {
                    name: issuer.name.clone(),
                    key,
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
            .with_context(|| {
                format!(
                    "enrollment token issuer {} is not configured",
                    loggable_claim(&issuer)
                )
            })?;
        // The token chooses its algorithm only among those of the configured
        // key's type.
        let mut validation = Validation::new_for_family(issuer.key.family());
        // Cannot fail while the key is selected by `iss`. Kept so that
        // selecting keys another way (such as by `kid`) still binds the token
        // to its issuer.
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

fn public_key(pem: &[u8]) -> anyhow::Result<DecodingKey> {
    DecodingKey::from_ec_pem(pem)
        .or_else(|_| DecodingKey::from_ed_pem(pem))
        .or_else(|_| DecodingKey::from_rsa_pem(pem))
        .context("expected a PEM-encoded ECDSA, Ed25519 or RSA public key")
}

/// An unverified claim, quoted with control characters escaped and cut to a
/// bounded length, since any caller chooses it and it ends up in the log.
fn loggable_claim(value: &str) -> String {
    const MAX_CHARS: usize = 64;
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(MAX_CHARS).collect();
    let ellipsis = if chars.next().is_some() { "…" } else { "" };
    format!("{prefix:?}{ellipsis}")
}

#[cfg(test)]
pub(crate) mod tests {
    use agentdesktop_core::config::{
        ControllerEnrollmentTokenIssuer, ControllerEnrollmentTokensConfig,
    };
    use aws_lc_rs::encoding::{AsDer, Pkcs8V1Der};
    use jsonwebtoken::{Algorithm, AlgorithmFamily, EncodingKey, Header};
    use serde_json::json;

    use super::{EnrollmentToken, EnrollmentTokens};

    pub(crate) struct Issuer {
        key: EncodingKey,
        algorithm: Algorithm,
        directory: tempfile::TempDir,
    }

    impl Issuer {
        pub(crate) fn new() -> Self {
            Self::with_key(rcgen::KeyPair::generate().unwrap(), Algorithm::ES256)
        }

        fn with_key(key: rcgen::KeyPair, algorithm: Algorithm) -> Self {
            let pem = key.serialize_pem();
            let directory = tempfile::tempdir().unwrap();
            std::fs::write(directory.path().join("jamf.pub.pem"), key.public_key_pem()).unwrap();
            Self {
                key: match algorithm.family() {
                    AlgorithmFamily::Ec => EncodingKey::from_ec_pem(pem.as_bytes()),
                    AlgorithmFamily::Ed => EncodingKey::from_ed_pem(pem.as_bytes()),
                    _ => EncodingKey::from_rsa_pem(pem.as_bytes()),
                }
                .unwrap(),
                algorithm,
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
            self.sign_with(self.algorithm, claims)
        }

        fn sign_with(&self, algorithm: Algorithm, claims: serde_json::Value) -> String {
            jsonwebtoken::encode(&Header::new(algorithm), &claims, &self.key).unwrap()
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
    fn the_configured_key_decides_which_algorithms_are_accepted() {
        let claims = || json!({"iss": "jamf", "aud": "fleet", "jti": "batch-1", "iat": 1});
        let p384 = || rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        let ed25519 = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519).unwrap();
        for (key, algorithm) in [(p384(), Algorithm::ES384), (ed25519, Algorithm::EdDSA)] {
            let issuer = Issuer::with_key(key, algorithm);
            let verified = issuer.tokens(&[]).verify(&issuer.sign(claims()));
            assert!(verified.is_ok(), "{algorithm:?}: {verified:?}");
        }

        let rsa = aws_lc_rs::rsa::KeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let rsa: Pkcs8V1Der = rsa.as_der().unwrap();
        let rsa = rcgen::KeyPair::try_from(rsa.as_ref()).unwrap();
        let rsa = Issuer::with_key(rsa, Algorithm::RS256);
        for algorithm in [Algorithm::RS256, Algorithm::RS384, Algorithm::PS512] {
            let verified = rsa.tokens(&[]).verify(&rsa.sign_with(algorithm, claims()));
            assert!(verified.is_ok(), "{algorithm:?}: {verified:?}");
        }

        // ES256 is ECDSA with P-256, so a P-384 key does not verify it.
        let p384 = Issuer::with_key(p384(), Algorithm::ES384);
        let token = Issuer::new().sign(claims());
        assert!(p384.tokens(&[]).verify(&token).is_err());
    }

    #[test]
    fn an_unknown_issuer_is_reported_escaped_and_bounded() {
        let issuer = Issuer::new();
        let forged = format!("jamf\nFORGED LOG LINE{}", "x".repeat(1000));
        let error = issuer
            .tokens(&[])
            .verify(&issuer.sign(json!({"iss": forged, "aud": "fleet", "jti": "a", "iat": 1})))
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(!message.contains('\n'), "{message}");
        assert!(message.contains(r#""jamf\nFORGED LOG LINE"#), "{message}");
        assert!(message.chars().count() < 200, "{message}");
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
