//! Enrollment with a controller-issued token, for devices provisioned by an
//! MDM. No user signs in: the daemon reads the token the MDM dropped next to
//! its configuration and the controller issues a device certificate for it.
//! The identity has no OIDC tokens; the controller authenticates the device by
//! its certificate alone.

use std::path::Path;

use agentdesktop_core::config::ControllerConnectionConfig;
use agentdesktop_proto::fleet::TokenEnrollmentRequest;
use anyhow::{Context, bail};

use crate::{enrollment::EnrollmentState, identity::Identity, oidc, remote};

pub(crate) async fn enroll(
    controller: &ControllerConnectionConfig,
    enrollment: &EnrollmentState,
    token_path: &Path,
) -> anyhow::Result<Identity> {
    let enrollment_token = read_token(token_path)?;
    enrollment.set("enrolling").await;
    let (device_key, csr) = oidc::device_csr()?;
    let mut client = remote::client(controller, None).await?;
    let response = client
        .enroll_with_token(TokenEnrollmentRequest {
            hostname: remote::hostname(),
            certificate_signing_request_der: csr.der().as_ref().to_vec(),
            enrollment_token,
        })
        .await
        .context("enroll with enrollment token")?
        .into_inner();
    let client_certificate_pem = String::from_utf8(response.client_certificate_pem)
        .context("controller returned a non-UTF-8 device certificate")?;
    if client_certificate_pem.is_empty() {
        bail!("controller returned an empty device certificate");
    }
    Ok(Identity {
        device_id: response.device_id,
        client_certificate_pem,
        client_private_key_pem: device_key.serialize_pem(),
        client_certificate_expires_at_unix_seconds: response
            .client_certificate_expires_at_unix_seconds,
        oauth: None,
        oauth_token_endpoint: String::new(),
        oauth_client_id: String::new(),
    })
}

fn read_token(path: &Path) -> anyhow::Result<String> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read enrollment token from {}", path.display()))?;
    let token = contents.trim();
    if token.is_empty() {
        bail!("enrollment token file {} is empty", path.display());
    }
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::read_token;

    #[test]
    fn reads_the_trimmed_token_and_rejects_an_empty_file() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-enrollment-token-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("token");
        std::fs::write(&path, " secret-token \n").unwrap();
        assert_eq!(read_token(&path).unwrap(), "secret-token");
        std::fs::write(&path, "\n").unwrap();
        assert!(read_token(&path).is_err());
        assert!(read_token(&directory.join("missing")).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
