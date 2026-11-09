use async_trait::async_trait;

use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::json;

use super::super::{
    Error, Provider, RequestMode,
    authentication::{LOCAL_SUBJECT, Principal},
    authorization::{AccessRequest, permission_names},
};

/// Local access grants every operation within the loopback listener boundary.
/// Cross-site browser mutations are rejected by the router's origin check, as for
/// every provider. This provider must never be selected for an authenticated/public
/// listener.
pub struct LocalAccess;

#[async_trait]
impl Provider for LocalAccess {
    fn routes(&self) -> Router {
        Router::new()
            .route(
                "/auth/session",
                get(|| async {
                    Json(json!({
                        "mode":"local", "authenticated":true, "role":"operator",
                        "subject":LOCAL_SUBJECT, "assurance":null,
                        "recent_auth_required":false, "permissions":permission_names()
                    }))
                }),
            )
            .route("/auth/activity", post(|| async { StatusCode::NO_CONTENT }))
    }

    async fn authenticate(
        &self,
        _headers: &HeaderMap,
        _mode: RequestMode,
    ) -> Result<Option<Principal>, Error> {
        Ok(Some(Principal::local()))
    }

    async fn check(
        &self,
        _principal: &Principal,
        _headers: &HeaderMap,
        _access: AccessRequest<'_>,
    ) -> Result<(), Error> {
        Ok(())
    }
}
