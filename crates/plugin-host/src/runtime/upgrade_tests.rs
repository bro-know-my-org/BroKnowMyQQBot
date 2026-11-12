//! Upgrade cancellation at the exact post-migration activation boundaries.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activation {
    Ready,
    PendingInit,
    PendingHealth,
    FailInit,
    FailHealth,
}

struct UpgradeProbe {
    manifest: PluginManifest,
    activation: Activation,
    starts: AtomicUsize,
    stops: AtomicUsize,
    health_checks: AtomicUsize,
}

impl UpgradeProbe {
    fn new(state_version: u32, activation: Activation) -> Arc<Self> {
        let mut manifest = builtin_plugins::PingPlugin::default().manifest().clone();
        manifest.state_version = state_version;
        manifest.permissions.storage = plugin_api::StoragePermission::Private;
        manifest.permissions.storage_quota_bytes = 4096;
        Arc::new(Self {
            manifest,
            activation,
            starts: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
            health_checks: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl StaticPlugin for UpgradeProbe {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn init(&self, _: InitContext) -> Result<(), PluginError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        match self.activation {
            Activation::PendingInit => std::future::pending().await,
            Activation::FailInit => Err(PluginError::Permanent("rejected init".to_owned())),
            _ => Ok(()),
        }
    }

    async fn health(&self) -> Result<HealthStatus, PluginError> {
        self.health_checks.fetch_add(1, Ordering::SeqCst);
        match self.activation {
            Activation::PendingHealth => std::future::pending().await,
            Activation::FailHealth => Ok(HealthStatus::Degraded),
            _ => Ok(HealthStatus::Healthy),
        }
    }

    async fn shutdown(&self) -> Result<(), PluginError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn migrate_state(
        &self,
        from: u32,
        to: u32,
        state: &BTreeMap<String, StateValue>,
    ) -> Result<Vec<StateOp>, PluginError> {
        assert_eq!((from, to), (1, 2));
        Ok(vec![
            StateOp::Put {
                key: "value".to_owned(),
                value: b"v2".to_vec(),
                expected_revision: Some(state["value"].revision),
            },
            StateOp::Delete {
                key: "removed".to_owned(),
                expected_revision: Some(state["removed"].revision),
            },
            StateOp::Put {
                key: "added".to_owned(),
                value: b"new".to_vec(),
                expected_revision: None,
            },
        ])
    }

    async fn on_event(
        &self,
        _: &PluginEventEnvelope,
        _: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        panic!("disabled old guest must never receive events")
    }
}

async fn upgrade_fixture(store: PluginStore) -> (StaticPluginHost, Arc<UpgradeProbe>) {
    store
        .commit(
            "upgrade",
            "seed",
            &["value", "removed"].map(|key| StateOp::Put {
                key: key.to_owned(),
                value: b"v1".to_vec(),
                expected_revision: None,
            }),
            &[],
            CommitOptions::new(4096),
        )
        .unwrap();
    let old = UpgradeProbe::new(1, Activation::Ready);
    let mut host = StaticPluginHost::new(store);
    host.register_trusted(old.clone(), "upgrade", BTreeMap::new())
        .await
        .unwrap();
    (host, old)
}

#[tokio::test]
async fn cancelled_upgrade_restores_persisted_state_and_never_reuses_old_guest() {
    for activation in [Activation::PendingInit, Activation::PendingHealth] {
        let path = std::env::temp_dir().join(format!("bkm-upgrade-{}.db", Uuid::new_v4()));
        let store = PluginStore::open(&path).unwrap();
        let (mut host, old) = upgrade_fixture(store.clone()).await;
        let original = store.snapshot("upgrade").unwrap();
        let candidate = UpgradeProbe::new(2, activation);
        {
            let upgrade = host.upgrade_trusted("upgrade", candidate.clone(), BTreeMap::new());
            tokio::pin!(upgrade);
            assert!(futures_util::poll!(&mut upgrade).is_pending());
            assert_eq!(store.snapshot("upgrade").unwrap()["value"].value, b"v2");
            assert_eq!(candidate.starts.load(Ordering::SeqCst), 1);
            assert_eq!(
                candidate.health_checks.load(Ordering::SeqCst),
                usize::from(activation == Activation::PendingHealth)
            );
        }
        assert_eq!(store.snapshot("upgrade").unwrap(), original);
        // A separate SQLite connection checks that rollback was durably committed.
        assert_eq!(
            PluginStore::open(&path)
                .unwrap()
                .snapshot("upgrade")
                .unwrap(),
            original
        );
        assert_eq!(
            host.plugins[0].state().unwrap(),
            PluginInstanceState::Disabled
        );
        assert_eq!(host.plugins[0].manifest.state_version, 1);
        assert!(host.set_instance_enabled("upgrade", true).await.is_err());
        host.shutdown().await.unwrap();
        assert_eq!(old.starts.load(Ordering::SeqCst), 1);
        assert_eq!(old.stops.load(Ordering::SeqCst), 1);
        drop(host);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}

#[tokio::test]
async fn explicit_upgrade_failure_restores_state_and_restarts_old_guest() {
    for activation in [Activation::FailInit, Activation::FailHealth] {
        let store = PluginStore::in_memory().unwrap();
        let (mut host, old) = upgrade_fixture(store.clone()).await;
        let original = store.snapshot("upgrade").unwrap();
        assert!(
            host.upgrade_trusted("upgrade", UpgradeProbe::new(2, activation), BTreeMap::new())
                .await
                .is_err()
        );
        assert_eq!(store.snapshot("upgrade").unwrap(), original);
        assert_eq!(host.plugins[0].state().unwrap(), PluginInstanceState::Ready);
        assert_eq!(host.plugins[0].manifest.state_version, 1);
        assert_eq!(old.starts.load(Ordering::SeqCst), 2);
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn rollback_storage_failure_keeps_old_guest_disabled() {
    for activation in [Activation::PendingInit, Activation::PendingHealth] {
        let path = std::env::temp_dir().join(format!("bkm-upgrade-{}.db", Uuid::new_v4()));
        let store = PluginStore::open(&path).unwrap();
        let (mut host, old) = upgrade_fixture(store.clone()).await;
        {
            let upgrade =
                host.upgrade_trusted("upgrade", UpgradeProbe::new(2, activation), BTreeMap::new());
            tokio::pin!(upgrade);
            assert!(futures_util::poll!(&mut upgrade).is_pending());
            rusqlite::Connection::open(&path)
                .unwrap()
                .execute_batch(
                    "CREATE TRIGGER reject_rollback BEFORE DELETE ON plugin_state
                     BEGIN SELECT RAISE(FAIL, 'rollback unavailable'); END;",
                )
                .unwrap();
        }
        assert_eq!(
            host.plugins[0].state().unwrap(),
            PluginInstanceState::Disabled
        );
        assert!(host.set_instance_enabled("upgrade", true).await.is_err());
        host.shutdown().await.unwrap();
        assert_eq!(old.starts.load(Ordering::SeqCst), 1);
        assert_eq!(old.stops.load(Ordering::SeqCst), 1);
        drop(host);
        drop(store);
        std::fs::remove_file(path).unwrap();
    }
}

#[tokio::test]
async fn successful_upgrade_keeps_migrated_state_and_publishes_candidate() {
    let store = PluginStore::in_memory().unwrap();
    let (mut host, old) = upgrade_fixture(store.clone()).await;
    let candidate = UpgradeProbe::new(2, Activation::Ready);
    host.upgrade_trusted("upgrade", candidate.clone(), BTreeMap::new())
        .await
        .unwrap();
    let state = store.snapshot("upgrade").unwrap();
    assert_eq!(state["value"].value, b"v2");
    assert_eq!(state["value"].revision, 2);
    assert_eq!(state["added"].value, b"new");
    assert!(!state.contains_key("removed"));
    assert_eq!(host.plugins[0].state().unwrap(), PluginInstanceState::Ready);
    assert_eq!(host.plugins[0].manifest.state_version, 2);
    host.shutdown().await.unwrap();
    assert_eq!(old.starts.load(Ordering::SeqCst), 1);
    assert_eq!(old.stops.load(Ordering::SeqCst), 1);
    assert_eq!(candidate.stops.load(Ordering::SeqCst), 1);
}
