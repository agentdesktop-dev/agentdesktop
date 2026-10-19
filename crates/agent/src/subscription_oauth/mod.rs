use std::{
    net::SocketAddr,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::Context;
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::Mutex;
use url::Url;

use crate::{oidc, secret_store::SecretStore, subscription::SubscriptionCredential};

pub mod anthropic;
pub(crate) mod openai;

const PROMPT_URI: &str = "http://localhost:51327/callback";
const EXPIRY_SKEW_SECONDS: u64 = 60;

struct LoginState {
    lock: Mutex<()>,
    declined: AtomicBool,
}

impl LoginState {
    const fn new() -> Self {
        Self {
            lock: Mutex::const_new(()),
            declined: AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
trait OAuthProvider: Send + Sync {
    type Token: Serialize + DeserializeOwned + Send + Sync;

    const NAME: &'static str;
    const SERVICE: &'static str;
    const ACCOUNT: &'static str;
    const REDIRECT_URI: &'static str;

    fn authorization_url(&self, redirect: &Url, state: &str, challenge: &str) -> Url;
    fn expires_at(&self, token: &Self::Token) -> u64;
    fn credential_from_token(&self, token: Self::Token) -> SubscriptionCredential;
    async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        state: &str,
    ) -> anyhow::Result<Self::Token>;
    async fn refresh(&self, _token: &Self::Token) -> anyhow::Result<Option<Self::Token>> {
        Ok(None)
    }
}

async fn credential<P: OAuthProvider + 'static>(
    provider: &'static P,
    login: &'static LoginState,
    state_dir: &Path,
    callback_listen: Option<SocketAddr>,
    open_browser: bool,
) -> anyhow::Result<Option<SubscriptionCredential>> {
    let state_dir = state_dir.to_owned();
    tokio::spawn(async move {
        credential_inner(provider, login, &state_dir, callback_listen, open_browser).await
    })
    .await
    .with_context(|| format!("join {} subscription OAuth task", P::NAME))?
}

async fn credential_inner<P: OAuthProvider>(
    provider: &P,
    login: &LoginState,
    state_dir: &Path,
    callback_listen: Option<SocketAddr>,
    open_browser: bool,
) -> anyhow::Result<Option<SubscriptionCredential>> {
    let _login = login.lock.lock().await;
    let store = SecretStore::new(state_dir)?;
    let prompt = Url::parse(PROMPT_URI).expect("valid subscription prompt URI");
    if let Some(stored) = load::<P>(&store)? {
        let ready = if provider.expires_at(&stored) > now().saturating_add(EXPIRY_SKEW_SECONDS) {
            Some(stored)
        } else if let Some(refreshed) = provider.refresh(&stored).await? {
            save::<P>(&store, &refreshed)?;
            Some(refreshed)
        } else {
            None
        };
        if let Some(token) = ready {
            if !open_browser {
                oidc::continue_subscription_page(&prompt, callback_listen, true).await?;
            }
            return Ok(Some(provider.credential_from_token(token)));
        }
    }
    if login.declined.load(Ordering::Relaxed) {
        if !open_browser {
            oidc::continue_subscription_page(&prompt, callback_listen, false).await?;
        }
        return Ok(None);
    }
    let redirect =
        Url::parse(P::REDIRECT_URI).context("parse subscription provider callback URI")?;
    let state = oidc::random_secret();
    let (verifier, challenge) = oidc::pkce();
    let authorization_url = provider.authorization_url(&redirect, &state, &challenge);
    let code = if redirect == prompt {
        oidc::wait_for_authorization_code_with_page(
            authorization_url.as_str(),
            &redirect,
            state.clone(),
            callback_listen,
            oidc::AuthorizationPage::Subscription,
            open_browser,
        )
        .await?
    } else {
        oidc::wait_for_subscription_authorization_code(
            authorization_url.as_str(),
            &redirect,
            state.clone(),
            callback_listen,
            open_browser,
        )
        .await?
    };
    let Some(code) = code else {
        login.declined.store(true, Ordering::Relaxed);
        return Ok(None);
    };
    let token = provider.exchange(&code, &verifier, &state).await?;
    save::<P>(&store, &token)?;
    Ok(Some(provider.credential_from_token(token)))
}

fn load<P: OAuthProvider>(store: &SecretStore) -> anyhow::Result<Option<P::Token>> {
    store
        .get_optional(P::SERVICE, P::ACCOUNT)?
        .map(|value| {
            serde_json::from_str(&value)
                .with_context(|| format!("decode stored {} subscription", P::NAME))
        })
        .transpose()
}

fn save<P: OAuthProvider>(store: &SecretStore, token: &P::Token) -> anyhow::Result<()> {
    store.set(
        P::SERVICE,
        P::ACCOUNT,
        &serde_json::to_string(token)
            .with_context(|| format!("encode {} subscription", P::NAME))?,
    )
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use agentdesktop_core::model::LlmGatewayCredential;
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Deserialize, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Token {
        access_token: String,
        expires_at_unix_seconds: u64,
    }

    #[derive(Default)]
    struct TestProvider {
        refreshes: AtomicUsize,
        fail_refresh: bool,
    }

    #[async_trait::async_trait]
    impl OAuthProvider for TestProvider {
        type Token = Token;
        const NAME: &'static str = "test-provider";
        const SERVICE: &'static str = "dev.agentdesktop.test-subscription";
        const ACCOUNT: &'static str = "test";
        const REDIRECT_URI: &'static str = PROMPT_URI;

        fn authorization_url(&self, _: &Url, _: &str, _: &str) -> Url {
            panic!("test must not open provider authorization")
        }
        fn expires_at(&self, token: &Token) -> u64 {
            token.expires_at_unix_seconds
        }
        fn credential_from_token(&self, token: Token) -> SubscriptionCredential {
            SubscriptionCredential {
                token: LlmGatewayCredential {
                    credential: token.access_token,
                    expires_at_unix_seconds: token.expires_at_unix_seconds,
                },
                account_id: None,
            }
        }
        async fn exchange(&self, _: &str, _: &str, _: &str) -> anyhow::Result<Token> {
            panic!("test must not exchange an authorization code")
        }
        async fn refresh(&self, _: &Token) -> anyhow::Result<Option<Token>> {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            if self.fail_refresh {
                anyhow::bail!("transient refresh failure");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(Some(Token {
                access_token: "refreshed".to_owned(),
                expires_at_unix_seconds: now() + 3600,
            }))
        }
    }

    #[tokio::test]
    async fn cached_credentials_are_reused_and_concurrent_expiry_refreshes_once() {
        let directory = tempfile::tempdir().unwrap();
        let store = SecretStore::new(directory.path()).unwrap();
        let provider = TestProvider::default();
        let login = LoginState::new();
        save::<TestProvider>(
            &store,
            &Token {
                access_token: "cached".to_owned(),
                expires_at_unix_seconds: now() + 3600,
            },
        )
        .unwrap();
        let cached = credential_inner(&provider, &login, directory.path(), None, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached.token.credential, "cached");
        assert_eq!(provider.refreshes.load(Ordering::SeqCst), 0);
        save::<TestProvider>(
            &store,
            &Token {
                access_token: "expired".to_owned(),
                expires_at_unix_seconds: 1,
            },
        )
        .unwrap();
        let (first, second) = tokio::join!(
            credential_inner(&provider, &login, directory.path(), None, true),
            credential_inner(&provider, &login, directory.path(), None, true),
        );
        assert_eq!(first.unwrap().unwrap().token.credential, "refreshed");
        assert_eq!(second.unwrap().unwrap().token.credential, "refreshed");
        assert_eq!(provider.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(
            load::<TestProvider>(&store).unwrap().unwrap().access_token,
            "refreshed"
        );
    }

    #[tokio::test]
    async fn failed_refresh_preserves_the_stored_credential() {
        let directory = tempfile::tempdir().unwrap();
        let store = SecretStore::new(directory.path()).unwrap();
        let provider = TestProvider {
            fail_refresh: true,
            ..TestProvider::default()
        };
        save::<TestProvider>(
            &store,
            &Token {
                access_token: "expired".to_owned(),
                expires_at_unix_seconds: 1,
            },
        )
        .unwrap();
        let before = store
            .get(TestProvider::SERVICE, TestProvider::ACCOUNT)
            .unwrap();
        assert!(
            credential_inner(&provider, &LoginState::new(), directory.path(), None, true)
                .await
                .is_err()
        );
        assert_eq!(
            store
                .get(TestProvider::SERVICE, TestProvider::ACCOUNT)
                .unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn declined_sign_in_is_remembered_without_opening_authorization() {
        let directory = tempfile::tempdir().unwrap();
        let login = LoginState::new();
        login.declined.store(true, Ordering::Relaxed);
        let provider = TestProvider::default();
        assert!(
            credential_inner(&provider, &login, directory.path(), None, true)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(provider.refreshes.load(Ordering::SeqCst), 0);
        assert!(!LoginState::new().declined.load(Ordering::Relaxed));
    }
}
