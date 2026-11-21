use std::sync::Arc;

use agentdesktop_core::model::EnrollmentStatus;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct EnrollmentState {
    status: Arc<RwLock<EnrollmentStatus>>,
}

/// Who a sign-in is for, which decides how the pending sign-in is reported.
#[derive(Clone, Copy)]
pub enum LoginPrompt<'a> {
    /// The sign-in enrolls the device.
    Enrollment(&'a EnrollmentState),
    /// A user signs in on a device that is already enrolled.
    User(&'a EnrollmentState),
}

impl LoginPrompt<'_> {
    /// Reports the page where the user signs in, and for device authorization
    /// the code to confirm there.
    pub async fn show(self, authorization_url: String, user_code: Option<String>) {
        match self {
            Self::Enrollment(enrollment) => {
                *enrollment.status.write().await = EnrollmentStatus {
                    status: "awaitingAuthentication".to_owned(),
                    authorization_url: Some(authorization_url),
                    user_code,
                    user: None,
                };
            }
            Self::User(enrollment) => {
                let mut status = enrollment.status.write().await;
                // The device may have lost its enrollment meanwhile.
                if status.status == "enrolled" {
                    status.authorization_url = Some(authorization_url);
                    status.user_code = user_code;
                    status.user = Some("awaitingAuthentication".to_owned());
                }
            }
        }
    }
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
                user: None,
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
            user: None,
        };
    }

    /// Ends a user's pending sign-in on an enrolled device, however it ended.
    pub async fn user_sign_in_ended(&self) {
        let mut status = self.status.write().await;
        if status.user.take().is_some() {
            status.authorization_url = None;
            status.user_code = None;
        }
    }
}
