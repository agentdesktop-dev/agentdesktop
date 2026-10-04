use std::sync::Arc;

use agentdesktop_core::model::EnrollmentStatus;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct EnrollmentState {
    status: Arc<RwLock<EnrollmentStatus>>,
}

impl EnrollmentState {
    pub fn new(configured: bool) -> Self {
        Self {
            status: Arc::new(RwLock::new(EnrollmentStatus {
                status: if configured {
                    "starting".to_owned()
                } else {
                    "notConfigured".to_owned()
                },
                authorization_url: None,
                user_code: None,
            })),
        }
    }

    pub async fn get(&self) -> EnrollmentStatus {
        self.status.read().await.clone()
    }

    pub async fn set(&self, status: &str) {
        *self.status.write().await = EnrollmentStatus {
            status: status.to_owned(),
            authorization_url: None,
            user_code: None,
        };
    }

    pub async fn awaiting_authentication(&self, authorization_url: String) {
        *self.status.write().await = EnrollmentStatus {
            status: "awaitingAuthentication".to_owned(),
            authorization_url: Some(authorization_url),
            user_code: None,
        };
    }

    pub async fn awaiting_device_authorization(&self, verification_url: String, user_code: String) {
        *self.status.write().await = EnrollmentStatus {
            status: "awaitingAuthentication".to_owned(),
            authorization_url: Some(verification_url),
            user_code: Some(user_code),
        };
    }
}
