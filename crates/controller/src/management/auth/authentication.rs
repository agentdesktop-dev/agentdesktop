use serde_json::{Map, Value};

pub const LOCAL_ISSUER: &str = "local";
pub const LOCAL_SUBJECT: &str = "localhost";

#[derive(Clone, Debug)]
/// Distinguishes local access, human sessions and app-only service tokens so grants
/// cannot accidentally cross credential types.
pub enum PrincipalKind {
    Local,
    Human,
    /// App-only credential. OAuth client identity (e.g. azp/appid) is distinct
    /// from the token's subject; service policy bindings check both.
    Service {
        client_id: String,
    },
}

/// Credential evidence supplied by a trusted, compiled-in authentication provider.
/// Contains no resolved roles: authorization is re-evaluated from this evidence.
#[derive(Clone, Debug)]
pub struct Principal {
    pub issuer: String,
    /// Verified subject identifier (`sub`), present for human and service credentials.
    /// Identifies the token's subject, not necessarily its OAuth client application.
    pub subject: String,
    pub kind: PrincipalKind,
    /// Verified claim snapshot, never unverified browser-supplied identity data.
    /// Local access has no claims (an empty map).
    pub claims: Map<String, Value>,
    /// Unix time of human authentication, used for recent-auth checks on mutations.
    /// Refresh must not advance this; absent when the provider cannot establish it.
    pub auth_time: Option<i64>,
    /// Unix time claims were last verified, for audit attribution. Unlike auth_time,
    /// this can advance on refresh; it does not establish recent human authentication.
    pub claims_verified_at: i64,
}

impl Principal {
    pub fn local() -> Self {
        Self {
            issuer: LOCAL_ISSUER.into(),
            subject: LOCAL_SUBJECT.into(),
            kind: PrincipalKind::Local,
            claims: Map::new(),
            auth_time: None,
            claims_verified_at: 0,
        }
    }

    pub fn is_service(&self) -> bool {
        matches!(self.kind, PrincipalKind::Service { .. })
    }
}
