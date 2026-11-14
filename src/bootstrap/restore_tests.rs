use super::*;
use crate::{
    backup::Repository,
    restore::{offline, plan::Prepared},
};
use plugin_api::{
    HandlerOutput, HostQueries, PluginError, PluginEventEnvelope, PluginManifest, StaticPlugin,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
struct Probe {
    manifest: PluginManifest,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    alive: Arc<AtomicBool>,
}
#[async_trait::async_trait]
impl StaticPlugin for Probe {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    async fn on_event(
        &self,
        _: &PluginEventEnvelope,
        _: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        Ok(HandlerOutput::default())
    }
    async fn shutdown(&self) -> Result<(), PluginError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(())
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn plugin_shutdown_gate_and_all_handles_finish_before_offline_application() {
    for cancel in [false, true] {
        let root = std::env::temp_dir().join(format!("bkm-drain-restore-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("plugins.db");
        let path = root.join("bot.toml");
        std::fs::write(&path, "[commands]\nprefix='!'\n").unwrap();
        let archive = plugin_host::LogicalArchive::create(
            BTreeMap::from([
                ("bot".into(), crate::backup::selected(&BotConfig::default())),
                ("bindings".into(), serde_json::json!([])),
            ]),
            plugin_host::LogicalSnapshot {
                installations: vec![],
                state: vec![],
            },
            BTreeMap::new(),
            1,
        )
        .unwrap();
        let store = PluginStore::open(&database).unwrap();
        let prepared = Prepared::new(
            archive,
            std::fs::read_to_string(&path).unwrap(),
            vec![],
            vec![],
            &[],
        )
        .unwrap();
        let accepted = offline::stage(
            Repository::new(&database, path.clone()),
            database,
            None,
            prepared,
        )
        .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let alive = Arc::new(AtomicBool::new(true));
        let probe = Arc::new(Probe {
            manifest: builtin_plugins::PingPlugin::default().manifest().clone(),
            entered: entered.clone(),
            release: release.clone(),
            alive: alive.clone(),
        });
        let mut host = StaticPluginHost::new(store.clone());
        host.register_trusted(probe, "fixture", BTreeMap::new())
            .await
            .unwrap();
        let manager = PluginManager::new(host, store, BTreeMap::new(), BTreeSet::new());
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut send = Some(send);
        let work = async {
            let signal = async { receive.await.map_err(std::io::Error::other) };
            tokio::pin!(signal);
            let restarting = finish_plugins(&manager, Ok(true), signal.as_mut())
                .await
                .unwrap();
            drop(manager);
            assert!(
                !alive.load(Ordering::SeqCst),
                "plugin and its DB handles must have dropped before application"
            );
            if restarting {
                offline::apply(&accepted).unwrap();
            } else {
                accepted.record_status("cancelled").unwrap();
            }
        };
        tokio::pin!(work);
        assert!(futures_util::poll!(&mut work).is_pending());
        entered.notified().await;
        assert!(alive.load(Ordering::SeqCst));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("prefix='!'")
        );
        if cancel {
            send.take().unwrap().send(()).unwrap();
        }
        assert!(futures_util::poll!(&mut work).is_pending());
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("prefix='!'")
        );
        release.notify_one();
        work.await;
        let config: BotConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.commands.prefix, if cancel { "!" } else { "/" });
        std::fs::remove_dir_all(root).unwrap();
    }
}
