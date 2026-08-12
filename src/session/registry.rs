use std::collections::HashMap;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::RwLock;

use crate::config::DeploymentMode;
use crate::service::hbone::{HboneClient, TlsRoots};

#[derive(Clone, Copy)]
pub(crate) enum RegistrationVersion {
    Managed(u64),
    #[cfg(target_os = "linux")]
    SelfManaged,
}

#[derive(Clone)]
pub struct SessionRegistry<Key> {
    mode: DeploymentMode,
    endpoint: SocketAddr,
    server_name: String,
    connect_timeout: Duration,
    roots: TlsRoots,
    inner: Arc<RegistryInner<Key>>,
}

struct RegistryInner<Key> {
    clients: RwLock<HashMap<Key, RegisteredClient>>,
    next_lease_id: AtomicU64,
}

struct RegisteredClient {
    managed_generation: Option<u64>,
    lease_id: u64,
    client: HboneClient,
}

impl<Key> SessionRegistry<Key> {
    pub fn new(
        mode: DeploymentMode,
        endpoint: SocketAddr,
        server_name: String,
        connect_timeout: Duration,
        roots: TlsRoots,
    ) -> Self {
        Self {
            mode,
            endpoint,
            server_name,
            connect_timeout,
            roots,
            inner: Arc::new(RegistryInner {
                clients: RwLock::new(HashMap::new()),
                next_lease_id: AtomicU64::new(1),
            }),
        }
    }

    pub(crate) fn mode(&self) -> DeploymentMode {
        self.mode
    }

    pub(crate) fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    pub(crate) fn server_name(&self) -> &str {
        &self.server_name
    }

    pub(crate) fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub(crate) fn roots(&self) -> &TlsRoots {
        &self.roots
    }
}

impl<Key> SessionRegistry<Key>
where
    Key: Clone + Eq + Hash + Send + Sync + 'static,
{
    pub(crate) async fn install(
        &self,
        key: Key,
        version: RegistrationVersion,
        client: HboneClient,
        monitor: tokio::task::JoinHandle<Result<()>>,
    ) -> Result<()> {
        let managed_generation = match version {
            RegistrationVersion::Managed(generation) => Some(generation),
            #[cfg(target_os = "linux")]
            RegistrationVersion::SelfManaged => None,
        };
        let lease_id = self.inner.next_lease_id.fetch_add(1, Ordering::Relaxed);
        let mut clients = self.inner.clients.write().await;
        if let Some(generation) = managed_generation
            && clients
                .get(&key)
                .and_then(|current| current.managed_generation)
                .is_some_and(|current| current >= generation)
        {
            anyhow::bail!("session certificate generation is not newer");
        }
        clients.insert(
            key.clone(),
            RegisteredClient {
                managed_generation,
                lease_id,
                client,
            },
        );
        drop(clients);

        let registry = self.clone();
        tokio::spawn(async move {
            let _ = monitor.await;
            registry.remove_lease(&key, lease_id).await;
        });
        Ok(())
    }

    pub(crate) async fn client(&self, key: &Key) -> Option<HboneClient> {
        self.inner
            .clients
            .read()
            .await
            .get(key)
            .map(|registered| registered.client.clone())
    }

    async fn remove_lease(&self, key: &Key, lease_id: u64) {
        let mut clients = self.inner.clients.write().await;
        if clients
            .get(key)
            .is_some_and(|registered| registered.lease_id == lease_id)
        {
            clients.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn client() -> HboneClient {
        HboneClient::connect("127.0.0.1:9".parse().unwrap())
            .await
            .unwrap()
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn old_self_managed_lease_cannot_remove_replacement() {
        let registry = SessionRegistry::new(
            DeploymentMode::Standalone,
            "127.0.0.1:9".parse().unwrap(),
            "unused.invalid".to_owned(),
            Duration::from_secs(1),
            TlsRoots::Native,
        );
        let (first_done, first_monitor) = tokio::sync::oneshot::channel();
        registry
            .install(
                1000_u32,
                RegistrationVersion::SelfManaged,
                client().await,
                tokio::spawn(async move {
                    first_monitor.await.unwrap();
                    Ok(())
                }),
            )
            .await
            .unwrap();
        let (_second_done, second_monitor) = tokio::sync::oneshot::channel::<()>();
        registry
            .install(
                1000_u32,
                RegistrationVersion::SelfManaged,
                client().await,
                tokio::spawn(async move {
                    second_monitor.await.unwrap();
                    Ok(())
                }),
            )
            .await
            .unwrap();

        first_done.send(()).unwrap();
        tokio::task::yield_now().await;
        assert!(registry.client(&1000).await.is_some());
    }

    #[tokio::test]
    async fn managed_registration_requires_newer_generation() {
        let registry = SessionRegistry::new(
            DeploymentMode::Managed,
            "127.0.0.1:9".parse().unwrap(),
            "gateway.test".to_owned(),
            Duration::from_secs(1),
            TlsRoots::Native,
        );
        registry
            .install(
                1000_u32,
                RegistrationVersion::Managed(2),
                client().await,
                tokio::spawn(std::future::pending()),
            )
            .await
            .unwrap();

        assert!(
            registry
                .install(
                    1000_u32,
                    RegistrationVersion::Managed(2),
                    client().await,
                    tokio::spawn(std::future::pending()),
                )
                .await
                .is_err()
        );
    }
}
