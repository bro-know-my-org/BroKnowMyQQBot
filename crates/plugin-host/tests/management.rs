use async_trait::async_trait;
use plugin_api::{
    Disposition, HandlerOutput, HostQueries, InitContext, PluginError, PluginEventEnvelope,
    PluginManifest, StateOp, StaticPlugin,
};
use plugin_host::{CommitOptions, PluginInstanceState, PluginStore, StaticPluginHost};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

struct Probe {
    manifest: PluginManifest,
    schema: Value,
    fail: AtomicBool,
    starts: AtomicUsize,
    stops: AtomicUsize,
}

#[async_trait]
impl StaticPlugin for Probe {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    fn config_schema(&self) -> Option<&Value> {
        Some(&self.schema)
    }
    async fn init(&self, _: InitContext) -> Result<(), PluginError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) {
            return Err(PluginError::Permanent("failed initialization".to_owned()));
        }
        Ok(())
    }
    async fn shutdown(&self) -> Result<(), PluginError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn on_event(
        &self,
        _: &PluginEventEnvelope,
        _: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        Ok(HandlerOutput {
            disposition: Disposition::Ignore,
            ..HandlerOutput::default()
        })
    }
}

#[tokio::test]
async fn stopping_preserves_state_and_a_failed_resume_cannot_be_reenabled() {
    let manifest = PluginManifest::from_toml(
        r#"
manifest_version = 1
id = "dev.bkm.management-test"
version = "0.1.0"
protocol = ">=1.0,<2.0"
[metadata]
default_locale = "en"
[metadata.locales.en]
name = "Management test"
description = "Management lifecycle fixture"
"#,
    )
    .unwrap();
    let plugin = Arc::new(Probe {
        manifest,
        schema: json!({"type":"object","properties":{"label":{"type":"string"}},"additionalProperties":false}),
        fail: AtomicBool::new(false),
        starts: AtomicUsize::new(0),
        stops: AtomicUsize::new(0),
    });
    let store = PluginStore::in_memory().unwrap();
    let mut host = StaticPluginHost::new(store.clone());
    host.register_trusted(plugin.clone(), "fixture", BTreeMap::new())
        .await
        .unwrap();
    store
        .commit(
            "fixture",
            "seed",
            &[StateOp::Put {
                key: "record".to_owned(),
                value: b"keep".to_vec(),
                expected_revision: None,
            }],
            &[],
            CommitOptions::new(1024),
        )
        .unwrap();
    assert!(
        host.configure_stopped_instance("fixture", BTreeMap::new())
            .await
            .is_err()
    );
    host.set_instance_enabled("fixture", false).await.unwrap();
    host.set_instance_enabled("fixture", false).await.unwrap();
    assert_eq!(plugin.stops.load(Ordering::SeqCst), 1);
    assert_eq!(
        host.instance_state("fixture").unwrap(),
        Some(PluginInstanceState::Stopped)
    );
    assert!(
        host.configure_stopped_instance(
            "fixture",
            BTreeMap::from([("label".to_owned(), json!(false))])
        )
        .await
        .is_err()
    );
    let config = BTreeMap::from([("label".to_owned(), json!("updated"))]);
    host.configure_stopped_instance("fixture", config.clone())
        .await
        .unwrap();
    host.set_instance_enabled("fixture", true).await.unwrap();
    assert_eq!(host.instance_snapshots().unwrap()[0].config, config);
    assert_eq!(plugin.starts.load(Ordering::SeqCst), 2);
    assert_eq!(
        store.state_get("fixture", "record").unwrap().unwrap().value,
        b"keep"
    );
    host.set_instance_enabled("fixture", false).await.unwrap();
    plugin.fail.store(true, Ordering::SeqCst);
    assert!(host.set_instance_enabled("fixture", true).await.is_err());
    assert_eq!(
        host.instance_state("fixture").unwrap(),
        Some(PluginInstanceState::Disabled)
    );
    host.set_instance_enabled("fixture", false).await.unwrap();
    assert!(host.set_instance_enabled("fixture", true).await.is_err());
    assert_eq!(plugin.starts.load(Ordering::SeqCst), 3);
    assert_eq!(plugin.stops.load(Ordering::SeqCst), 2);
    host.shutdown().await.unwrap();
}
