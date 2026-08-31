use std::{net::SocketAddr, path::Path};

use agentdesktop_core::model::LlmGatewayCredential;

use crate::anthropic_oauth;

pub async fn compose(
    identity: LlmGatewayCredential,
    state_dir: &Path,
    callback_listen: Option<SocketAddr>,
    continue_in_browser: bool,
) -> anyhow::Result<LlmGatewayCredential> {
    let subscription =
        anthropic_oauth::credential(state_dir, callback_listen, !continue_in_browser).await?;
    Ok(combine(subscription, identity))
}

fn combine(
    subscription: Option<LlmGatewayCredential>,
    identity: LlmGatewayCredential,
) -> LlmGatewayCredential {
    let Some(subscription) = subscription else {
        return identity;
    };
    LlmGatewayCredential {
        credential: format!(
            "agentdesktop:{}:{}",
            subscription.credential, identity.credential
        ),
        expires_at_unix_seconds: subscription
            .expires_at_unix_seconds
            .min(identity.expires_at_unix_seconds),
    }
}

#[cfg(test)]
mod tests {
    use agentdesktop_core::model::LlmGatewayCredential;

    use super::combine;

    #[test]
    fn credential_contains_subscription_then_identity_and_uses_earliest_expiry() {
        let credential = combine(
            Some(LlmGatewayCredential {
                credential: "subscription".to_owned(),
                expires_at_unix_seconds: 200,
            }),
            LlmGatewayCredential {
                credential: "identity".to_owned(),
                expires_at_unix_seconds: 100,
            },
        );
        assert_eq!(credential.credential, "agentdesktop:subscription:identity");
        assert_eq!(credential.expires_at_unix_seconds, 100);
    }

    #[test]
    fn skipped_subscription_uses_identity_credential() {
        let credential = combine(
            None,
            LlmGatewayCredential {
                credential: "identity".to_owned(),
                expires_at_unix_seconds: 100,
            },
        );
        assert_eq!(credential.credential, "identity");
        assert_eq!(credential.expires_at_unix_seconds, 100);
    }
}
