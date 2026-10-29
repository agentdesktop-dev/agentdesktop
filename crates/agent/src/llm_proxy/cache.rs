use std::{sync::Arc, time::Duration};

/// A cached controller credential is not used within this margin of its expiry.
const CREDENTIAL_EXPIRY_MARGIN: Duration = Duration::from_secs(30);
/// Longest a controller credential is served from the cache, counted from its
/// fetch. This bounds how long a revoked device keeps using the proxy:
/// revocation is enforced at issuance, the gateway validates tokens statelessly.
const CREDENTIAL_CACHE_TTL: Duration = Duration::from_secs(60);

/// Per-server cache of controller-issued gateway credentials, one per client
/// id. A controller fetch is a fresh mTLS connection and round trip; agent-mode
/// sessions issue bursts of requests, so a short cache keeps them off the
/// controller. Fetches are single-flight per key: concurrent misses wait for
/// the one fetch in progress and then read its result from the cache. The
/// entries lock is never held across a fetch, so a slow controller delays only
/// the requests waiting for that key, not cache hits on other routes.
/// OIDC credentials never go through here; the secret store is their cache.
#[derive(Default)]
pub(crate) struct CredentialCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, CachedCredential>>,
    flights: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

struct CachedCredential {
    credential: String,
    /// The enrolled device the credential was issued to. A logout or a
    /// re-enrollment changes it, and the entry is then ignored.
    device_id: String,
    /// Monotonic deadline (wall-clock expiry at fetch time, minus the margin,
    /// capped by CREDENTIAL_CACHE_TTL) and the wall-clock deadline itself. Both
    /// must be in the future: the monotonic clock does not advance across a
    /// suspend, the wall clock can jump; neither alone is trusted.
    valid_until: std::time::Instant,
    valid_until_unix: u64,
    /// The credential's own expiry (the controller JWT `exp`), on both clocks:
    /// a tunnel opened with the credential runs until then, not until the
    /// cache stops reusing it.
    expires: std::time::Instant,
    expires_unix: u64,
}

/// A point in time on both clocks (see `CachedCredential`): a tunnel closes
/// when either says the credential has expired.
#[derive(Clone, Copy, Debug)]
pub(super) struct Deadline {
    pub(super) monotonic: std::time::Instant,
    pub(super) unix: u64,
}

impl Deadline {
    pub(super) fn from_unix(expires_unix: u64) -> Self {
        Self {
            monotonic: std::time::Instant::now()
                + Duration::from_secs(expires_unix.saturating_sub(now_unix())),
            unix: expires_unix,
        }
    }
}

pub(super) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl CredentialCache {
    /// Reuse the proxy's bounded cache for local credential-helper requests.
    pub(crate) async fn get_credential<F, Fut, E>(
        &self,
        key: &str,
        device_id: &str,
        fetch: F,
    ) -> Result<agentdesktop_core::model::LlmGatewayCredential, E>
    where
        F: FnOnce() -> Fut,
        Fut:
            std::future::Future<Output = Result<agentdesktop_core::model::LlmGatewayCredential, E>>,
    {
        let (credential, expiry, _) = self.get_or_fetch(key, device_id, fetch).await?;
        Ok(agentdesktop_core::model::LlmGatewayCredential {
            credential,
            expires_at_unix_seconds: expiry.unix,
        })
    }

    /// The lock a fetch for the key holds; one per key, kept for the
    /// listener's lifetime (keys are client id, gateway and audience).
    fn flight(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key.to_owned())
            .or_default()
            .clone()
    }

    /// The cached credential for the key, or one from `fetch`, which is cached
    /// at once. Single-flight: a caller that finds a fetch in progress waits
    /// for it and reads its result. Returns the credential, its own expiry, and
    /// whether it came from the cache.
    pub(super) async fn get_or_fetch<F, Fut, E>(
        &self,
        key: &str,
        device_id: &str,
        fetch: F,
    ) -> Result<(String, Deadline, bool), E>
    where
        F: FnOnce() -> Fut,
        Fut:
            std::future::Future<Output = Result<agentdesktop_core::model::LlmGatewayCredential, E>>,
    {
        let flight = self.flight(key);
        let _flight = flight.lock().await;
        if let Some((credential, deadline)) = self.lookup(key, device_id) {
            return Ok((credential, deadline, true));
        }
        let fetched = fetch().await?;
        self.insert(key, device_id, &fetched);
        let deadline = Deadline::from_unix(fetched.expires_at_unix_seconds);
        Ok((fetched.credential, deadline, false))
    }

    /// A cached credential for the key and device, if one is still valid.
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn get(&self, key: &str, device_id: &str) -> Option<String> {
        self.lookup(key, device_id)
            .map(|(credential, _)| credential)
    }

    /// A cached credential together with its own expiry, read in one go so a
    /// tunnel opened with it always gets the deadline of the credential it
    /// was actually opened with.
    pub(super) fn lookup(&self, key: &str, device_id: &str) -> Option<(String, Deadline)> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries
            .get(key)
            .filter(|entry| {
                entry.device_id == device_id
                    && std::time::Instant::now() < entry.valid_until
                    && now_unix() < entry.valid_until_unix
            })
            .map(|entry| {
                (
                    entry.credential.clone(),
                    Deadline {
                        monotonic: entry.expires,
                        unix: entry.expires_unix,
                    },
                )
            })
    }

    /// Remember a freshly fetched credential until the earlier of its expiry
    /// margin and the cache TTL.
    pub(super) fn insert(
        &self,
        key: &str,
        device_id: &str,
        fetched: &agentdesktop_core::model::LlmGatewayCredential,
    ) {
        let now = now_unix();
        let remaining = Duration::from_secs(fetched.expires_at_unix_seconds.saturating_sub(now));
        let lifetime = remaining
            .saturating_sub(CREDENTIAL_EXPIRY_MARGIN)
            .min(CREDENTIAL_CACHE_TTL);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if lifetime > Duration::ZERO {
            entries.insert(
                key.to_owned(),
                CachedCredential {
                    credential: fetched.credential.clone(),
                    device_id: device_id.to_owned(),
                    valid_until: std::time::Instant::now() + lifetime,
                    valid_until_unix: now + lifetime.as_secs(),
                    expires: std::time::Instant::now() + remaining,
                    expires_unix: fetched.expires_at_unix_seconds,
                },
            );
        } else {
            tracing::debug!(
                key,
                "controller credential already within its expiry margin; not cached"
            );
            entries.remove(key);
        }
    }

    /// Drop the entry for the key if it still holds the given credential. A
    /// concurrent request that already replaced it with a fresh one is left alone.
    pub(super) fn invalidate_if(&self, key: &str, credential: &str) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if entries
            .get(key)
            .is_some_and(|entry| entry.credential == credential)
        {
            entries.remove(key);
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
impl CredentialCache {
    pub(super) fn expire_at_for_test(&self, key: &str, deadline: Deadline) {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(key).expect("cached test credential");
        entry.expires = deadline.monotonic;
        entry.expires_unix = deadline.unix;
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_misses_share_one_fetch_per_key() {
        let far = now_unix() + 3600;
        let cache = Arc::new(CredentialCache::default());
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        for key in ["a", "a", "a", "a", "a", "b", "b"] {
            let cache = cache.clone();
            let fetches = fetches.clone();
            tasks.spawn(async move {
                cache
                    .get_or_fetch(key, "device-1", || async move {
                        let n = fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok::<_, ()>(agentdesktop_core::model::LlmGatewayCredential {
                            credential: format!("{key}{n}"),
                            expires_at_unix_seconds: far,
                        })
                    })
                    .await
                    .map(|(credential, _, _)| (key, credential))
            });
        }
        let mut results = Vec::new();
        while let Some(result) = tasks.join_next().await {
            results.push(result.unwrap().unwrap());
        }
        // One fetch per key; every caller of a key got that fetch's credential.
        assert_eq!(fetches.load(std::sync::atomic::Ordering::SeqCst), 2);
        for key in ["a", "b"] {
            let mut credentials: Vec<_> = results
                .iter()
                .filter(|(k, _)| *k == key)
                .map(|(_, c)| c)
                .collect();
            credentials.dedup();
            assert_eq!(credentials.len(), 1, "{key}: {results:?}");
        }
        // Cached at fetch time, before any gateway answer.
        assert!(cache.get("a", "device-1").is_some());
    }

    #[test]
    fn credential_cache_deadline_is_the_credential_expiry_not_the_reuse_window() {
        let cache = CredentialCache::default();
        let expires = now_unix() + 3600;
        cache.insert(
            "k",
            "device-1",
            &agentdesktop_core::model::LlmGatewayCredential {
                credential: "jwt".to_owned(),
                expires_at_unix_seconds: expires,
            },
        );
        let (credential, deadline) = cache.lookup("k", "device-1").expect("cached");
        assert_eq!(credential, "jwt");
        assert_eq!(deadline.unix, expires);
        let remaining = deadline.monotonic - std::time::Instant::now();
        assert!(
            remaining > CREDENTIAL_CACHE_TTL && remaining <= Duration::from_secs(3600),
            "tunnel deadline must follow the credential's expiry, got {remaining:?}"
        );
        // The reuse window is still the short one.
        let entry = cache.entries.lock().unwrap();
        assert!(entry["k"].valid_until <= std::time::Instant::now() + CREDENTIAL_CACHE_TTL);
    }

    #[test]
    fn credential_cache_honours_device_ttl_and_invalidation() {
        let cache = CredentialCache::default();
        let far = now_unix() + 600;
        let issued = |value: &str, expires: u64| agentdesktop_core::model::LlmGatewayCredential {
            credential: value.to_owned(),
            expires_at_unix_seconds: expires,
        };
        cache.insert("k", "device-1", &issued("one", far));
        assert_eq!(cache.get("k", "device-1").as_deref(), Some("one"));
        assert_eq!(
            cache.get("k", "device-2"),
            None,
            "another device never sees it"
        );
        assert_eq!(cache.get("other", "device-1"), None);
        cache.invalidate_if("k", "someone-else");
        assert_eq!(
            cache.get("k", "device-1").as_deref(),
            Some("one"),
            "a different credential leaves the entry alone"
        );
        cache.invalidate_if("k", "one");
        assert_eq!(cache.get("k", "device-1"), None);
        // Already within the expiry margin: not cached at all.
        cache.insert("m", "device-1", &issued("m1", now_unix() + 10));
        assert_eq!(cache.get("m", "device-1"), None);
        // Lifetime is capped by the TTL, not the token's own expiry.
        cache.insert("t", "device-1", &issued("t1", far));
        let entries = cache.entries.lock().unwrap();
        let entry = &entries["t"];
        assert!(entry.valid_until <= std::time::Instant::now() + CREDENTIAL_CACHE_TTL);
        assert!(entry.valid_until_unix <= now_unix() + CREDENTIAL_CACHE_TTL.as_secs());
    }
}
