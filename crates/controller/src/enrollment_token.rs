use std::collections::HashSet;

use agentdesktop_core::config::ControllerEnrollmentTokensConfig;
use anyhow::{Context, anyhow, bail};
use jsonwebtoken::{
    DecodingKey, Validation, decode, decode_header, errors::ErrorKind as JwtErrorKind,
};
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
    keys: Vec<IssuerKey>,
    /// Every key has an ID, so tokens must name theirs in `kid`.
    requires_kid: bool,
    revoked_ids: HashSet<String>,
}

#[derive(Clone)]
struct IssuerKey {
    id: Option<String>,
    key: DecodingKey,
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
                let keys = issuer
                    .keys()
                    .map(|(id, path)| {
                        let pem = std::fs::read(path)
                            .with_context(|| format!("read public key from {}", path.display()))?;
                        let key = public_key(&pem)
                            .with_context(|| format!("parse public key from {}", path.display()))?;
                        Ok(IssuerKey {
                            id: id.map(str::to_owned),
                            key,
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                if keys.is_empty() {
                    bail!("enrollment token issuer {} has no public keys", issuer.name);
                }
                Ok(Issuer {
                    name: issuer.name.clone(),
                    requires_kid: keys.iter().all(|key| key.id.is_some()),
                    keys,
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
        // Only selects the keys; the claims are verified below.
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
        let header = decode_header(token).context("decode enrollment token header")?;
        let candidates: Vec<&IssuerKey> = if issuer.requires_kid {
            let kid = header
                .kid
                .context("enrollment token has no kid, which its issuer requires")?;
            let key = issuer
                .keys
                .iter()
                .find(|key| key.id.as_deref() == Some(kid.as_str()))
                .with_context(|| {
                    format!(
                        "enrollment token key {} is not configured",
                        loggable_claim(&kid)
                    )
                })?;
            vec![key]
        } else {
            issuer
                .keys
                .iter()
                .filter(|key| key.key.family() == header.alg.family())
                .collect()
        };
        let mut errors = Vec::new();
        let claims = candidates.into_iter().find_map(|key| {
            decode::<Claims>(token, &key.key, &self.validation(issuer, &key.key))
                .map_err(|error| errors.push(error))
                .ok()
        });
        let Some(claims) = claims.map(|data| data.claims) else {
            // Prefer the error of a key whose signature matched: it names the
            // claim that failed.
            let error = match errors
                .iter()
                .position(|error| *error.kind() != JwtErrorKind::InvalidSignature)
            {
                Some(index) => Some(errors.swap_remove(index)),
                None => errors.pop(),
            };
            let error = error.map_or_else(
                || anyhow!("no public key of the token's issuer matches its algorithm"),
                anyhow::Error::new,
            );
            return Err(error.context("validate enrollment token"));
        };
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

    fn validation(&self, issuer: &Issuer, key: &DecodingKey) -> Validation {
        // The token chooses its algorithm only among those of the configured
        // key's type.
        let mut validation = Validation::new_for_family(key.family());
        // Cannot fail: only the keys of the issuer named by `iss` are tried.
        // Kept so that the token stays bound to its issuer if keys are ever
        // looked up another way.
        validation.set_issuer(&[&issuer.name]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["iss", "aud"]);
        validation.validate_nbf = true;
        validation
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
        ControllerEnrollmentTokenIssuer, ControllerEnrollmentTokenKey,
        ControllerEnrollmentTokensConfig,
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
                    public_key_path: Some(self.public_key_path()),
                    public_keys: Vec::new(),
                    revoked_ids: revoked_ids.iter().map(|id| (*id).to_owned()).collect(),
                }],
            })
            .unwrap()
        }

        fn public_key_path(&self) -> std::path::PathBuf {
            self.directory.path().join("jamf.pub.pem")
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

        fn token_with_kid(&self, kid: &str) -> String {
            let mut header = Header::new(self.algorithm);
            header.kid = Some(kid.to_owned());
            let claims = json!({"iss": "jamf", "aud": "fleet", "jti": "a", "iat": 1});
            jsonwebtoken::encode(&header, &claims, &self.key).unwrap()
        }
    }

    #[test]
    fn an_issuer_accepts_tokens_signed_by_any_of_its_keys() {
        let (old, new, other) = (Issuer::new(), Issuer::new(), Issuer::new());
        let tokens = |ids: [Option<&str>; 2]| {
            EnrollmentTokens::load(&ControllerEnrollmentTokensConfig {
                audience: "fleet".to_owned(),
                issuers: vec![ControllerEnrollmentTokenIssuer {
                    name: "jamf".to_owned(),
                    public_key_path: None,
                    public_keys: [&old, &new]
                        .into_iter()
                        .zip(ids)
                        .map(|(issuer, id)| ControllerEnrollmentTokenKey {
                            path: issuer.public_key_path(),
                            id: id.map(str::to_owned),
                        })
                        .collect(),
                    revoked_ids: Vec::new(),
                }],
            })
            .unwrap()
        };

        let any_key = tokens([None, None]);
        assert!(any_key.verify(&old.token("a")).is_ok());
        assert!(any_key.verify(&new.token("a")).is_ok());
        assert!(any_key.verify(&new.token_with_kid("ignored")).is_ok());
        assert!(any_key.verify(&other.token("a")).is_err());
        // The second key's error is reported: the first one's signature never matched.
        let expired =
            new.sign(json!({"iss": "jamf", "aud": "fleet", "jti": "a", "iat": 1, "exp": 1}));
        let error = any_key.verify(&expired).unwrap_err();
        assert!(
            format!("{error:#}").contains("ExpiredSignature"),
            "{error:#}"
        );

        let named_keys = tokens([Some("old"), Some("new")]);
        assert!(named_keys.verify(&old.token_with_kid("old")).is_ok());
        assert!(named_keys.verify(&new.token_with_kid("new")).is_ok());
        for (case, token) in [
            ("no kid", old.token("a")),
            ("unknown kid", old.token_with_kid("unknown")),
            ("another key's kid", old.token_with_kid("new")),
        ] {
            assert!(
                named_keys.verify(&token).is_err(),
                "accepted a token: {case}"
            );
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
