use std::{net::SocketAddr, path::Path};

use agentdesktop_core::model::LlmGatewayCredential;

use crate::{anthropic_oauth, openai_oauth};

pub struct SubscriptionCredential {
    pub token: LlmGatewayCredential,
    pub account_id: Option<String>,
}

pub async fn compose(
    identity: LlmGatewayCredential,
    state_dir: &Path,
    callback_listen: Option<SocketAddr>,
    continue_in_browser: bool,
    client_id: &str,
) -> anyhow::Result<LlmGatewayCredential> {
    let subscription =
        credential(state_dir, callback_listen, continue_in_browser, client_id).await?;
    Ok(combine(subscription, identity))
}

/// The subscription credential, or `None` when the user skipped signing in.
pub async fn credential(
    state_dir: &Path,
    callback_listen: Option<SocketAddr>,
    continue_in_browser: bool,
    client_id: &str,
) -> anyhow::Result<Option<SubscriptionCredential>> {
    match client_id {
        "claude-code" | "claude-desktop" => {
            Ok(
                anthropic_oauth::credential(state_dir, callback_listen, !continue_in_browser)
                    .await?
                    .map(|token| SubscriptionCredential {
                        token,
                        account_id: None,
                    }),
            )
        }
        "codex" => Ok(
            openai_oauth::credential(state_dir, callback_listen, !continue_in_browser)
                .await?
                .map(|(token, account_id)| SubscriptionCredential {
                    token,
                    account_id: Some(account_id),
                }),
        ),
        _ => anyhow::bail!("subscription authentication is not supported for client {client_id}"),
    }
}

pub fn combine(
    subscription: Option<SubscriptionCredential>,
    identity: LlmGatewayCredential,
) -> LlmGatewayCredential {
    let Some(subscription) = subscription else {
        return identity;
    };
    let mut credential = format!(
        "agentdesktop:{}:{}",
        subscription.token.credential, identity.credential
    );
    if let Some(account_id) = subscription.account_id {
        credential.push(':');
        credential.push_str(&account_id);
    }
    LlmGatewayCredential {
        credential,
        expires_at_unix_seconds: subscription
            .token
            .expires_at_unix_seconds
            .min(identity.expires_at_unix_seconds),
    }
}

#[cfg(test)]
mod tests {
    use agentdesktop_core::model::LlmGatewayCredential;

    use super::{SubscriptionCredential, combine};

    #[test]
    fn credential_contains_subscription_then_identity_and_uses_earliest_expiry() {
        let credential = combine(
            Some(SubscriptionCredential {
                token: LlmGatewayCredential {
                    credential: "subscription".to_owned(),
                    expires_at_unix_seconds: 200,
                },
                account_id: None,
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
    fn chatgpt_credential_keeps_identity_third_and_appends_account() {
        let credential = combine(
            Some(SubscriptionCredential {
                token: LlmGatewayCredential {
                    credential: "chatgpt-token".to_owned(),
                    expires_at_unix_seconds: 100,
                },
                account_id: Some("account-1".to_owned()),
            }),
            LlmGatewayCredential {
                credential: "identity".to_owned(),
                expires_at_unix_seconds: 200,
            },
        );
        assert_eq!(
            credential.credential,
            "agentdesktop:chatgpt-token:identity:account-1"
        );
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
