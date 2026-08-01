//! Deliberately failing BPP plugins used only by Host acceptance tests.

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use plugin_api::{
    CommandDeclaration, Disposition, HandlerOutput, HostQueries, PluginCommand, PluginError,
    PluginEventEnvelope, PluginId, PluginManifest, PluginMetadata, PluginPermissions,
    PluginRuntimeConfig, RuntimeMode, StateOp, StaticPlugin, StoragePermission, Subscription,
};
use serde_json::{Value, json};
use tokio::time::sleep;

#[derive(Debug)]
pub struct TimeoutFixturePlugin {
    manifest: PluginManifest,
}

impl Default for TimeoutFixturePlugin {
    fn default() -> Self {
        Self {
            manifest: fixture_manifest("dev.bkm.timeout-fixture", "Timeout Fixture", -20, 10),
        }
    }
}

#[async_trait]
impl StaticPlugin for TimeoutFixturePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        _event: &PluginEventEnvelope,
        _queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        sleep(Duration::from_millis(100)).await;
        Ok(HandlerOutput {
            disposition: Disposition::Ignore,
            ..HandlerOutput::default()
        })
    }
}

#[derive(Debug)]
pub struct PanicFixturePlugin {
    manifest: PluginManifest,
}

impl Default for PanicFixturePlugin {
    fn default() -> Self {
        Self {
            manifest: fixture_manifest("dev.bkm.panic-fixture", "Panic Fixture", -10, 1_000),
        }
    }
}

#[async_trait]
impl StaticPlugin for PanicFixturePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        _event: &PluginEventEnvelope,
        _queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        panic!("intentional static plugin fixture panic")
    }
}

#[derive(Debug)]
pub struct QuotaFixturePlugin {
    manifest: PluginManifest,
}

#[derive(Debug, Clone, Copy)]
enum MigrationBehavior {
    VersionOne,
    Success,
    FailMigration,
    FailInit,
}

#[derive(Debug)]
pub struct MigrationFixturePlugin {
    manifest: PluginManifest,
    behavior: MigrationBehavior,
}

#[derive(Debug, Default)]
pub struct PartitionStats {
    active: AtomicUsize,
    max_active: AtomicUsize,
    records: Mutex<Vec<String>>,
}

impl PartitionStats {
    pub fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    pub fn records(&self) -> Vec<String> {
        self.records.lock().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |records| records.clone(),
        )
    }
}

#[derive(Debug)]
pub struct PartitionFixturePlugin {
    manifest: PluginManifest,
    stats: Arc<PartitionStats>,
}

struct ActiveInvocationGuard(Arc<PartitionStats>);

impl Drop for ActiveInvocationGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

impl PartitionFixturePlugin {
    pub fn instrumented() -> (Self, Arc<PartitionStats>) {
        let mut manifest =
            fixture_manifest("dev.bkm.partition-fixture", "Partition Fixture", 0, 1_000);
        manifest.runtime.mode = RuntimeMode::Partitioned;
        manifest.runtime.max_concurrency = 2;
        manifest.permissions.storage = StoragePermission::Private;
        manifest.permissions.storage_quota_bytes = 1024;
        let stats = Arc::new(PartitionStats::default());
        (
            Self {
                manifest,
                stats: Arc::clone(&stats),
            },
            stats,
        )
    }
}

#[async_trait]
impl StaticPlugin for PartitionFixturePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        let group = event
            .payload
            .pointer("/data/target/group_id")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::Permanent("partition group is missing".to_owned()))?;
        let label = event
            .payload
            .pointer("/data/text")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::Permanent("partition label is missing".to_owned()))?;
        let active = self.stats.active.fetch_add(1, Ordering::SeqCst) + 1;
        let _active_guard = ActiveInvocationGuard(Arc::clone(&self.stats));
        self.stats.max_active.fetch_max(active, Ordering::SeqCst);
        self.record(format!("start:{group}:{label}"));
        sleep(Duration::from_millis(100)).await;
        self.record(format!("finish:{group}:{label}"));
        let key = format!("partition/{group}");
        let current = queries.state_get(&key);
        let count = current
            .and_then(|value| std::str::from_utf8(&value.value).ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_add(1);
        Ok(HandlerOutput {
            disposition: Disposition::Continue,
            state_ops: vec![StateOp::Put {
                key,
                value: count.to_string().into_bytes(),
                expected_revision: current.map(|value| value.revision),
            }],
            commands: Vec::new(),
            diagnostics: Vec::new(),
        })
    }
}

impl PartitionFixturePlugin {
    fn record(&self, value: String) {
        match self.stats.records.lock() {
            Ok(mut records) => records.push(value),
            Err(poisoned) => poisoned.into_inner().push(value),
        }
    }
}

impl MigrationFixturePlugin {
    pub fn version_one() -> Self {
        Self::new(1, MigrationBehavior::VersionOne)
    }

    pub fn version_two() -> Self {
        Self::new(2, MigrationBehavior::Success)
    }

    pub fn failing_migration() -> Self {
        Self::new(2, MigrationBehavior::FailMigration)
    }

    pub fn failing_init() -> Self {
        Self::new(2, MigrationBehavior::FailInit)
    }

    fn new(state_version: u32, behavior: MigrationBehavior) -> Self {
        let mut manifest =
            fixture_manifest("dev.bkm.migration-fixture", "Migration Fixture", 0, 1_000);
        manifest.state_version = state_version;
        manifest.permissions.storage = StoragePermission::Private;
        manifest.permissions.storage_quota_bytes = 1024;
        Self { manifest, behavior }
    }
}

#[async_trait]
impl StaticPlugin for MigrationFixturePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn init(&self, _context: plugin_api::InitContext) -> Result<(), PluginError> {
        if matches!(self.behavior, MigrationBehavior::FailInit) {
            Err(PluginError::Permanent(
                "intentional migration fixture init failure".to_owned(),
            ))
        } else {
            Ok(())
        }
    }

    async fn migrate_state(
        &self,
        from_version: u32,
        to_version: u32,
        state: &BTreeMap<String, plugin_api::StateValue>,
    ) -> Result<Vec<StateOp>, PluginError> {
        if matches!(self.behavior, MigrationBehavior::FailMigration) {
            return Err(PluginError::Permanent(
                "intentional migration fixture failure".to_owned(),
            ));
        }
        if from_version != 1 || to_version != 2 {
            return Err(PluginError::Permanent(
                "unexpected migration version range".to_owned(),
            ));
        }
        let current = state.get("value").ok_or_else(|| {
            PluginError::Permanent("migration source value is missing".to_owned())
        })?;
        Ok(vec![StateOp::Put {
            key: "value".to_owned(),
            value: b"v2".to_vec(),
            expected_revision: Some(current.revision),
        }])
    }

    async fn on_event(
        &self,
        _event: &PluginEventEnvelope,
        _queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        Ok(HandlerOutput {
            disposition: Disposition::Ignore,
            ..HandlerOutput::default()
        })
    }
}

impl Default for QuotaFixturePlugin {
    fn default() -> Self {
        let mut manifest = fixture_manifest("dev.bkm.quota-fixture", "Quota Fixture", 0, 1_000);
        manifest.commands.push(CommandDeclaration {
            name: "quota-fixture".to_owned(),
            aliases: Vec::new(),
            description: "Trigger an atomic storage quota rejection".to_owned(),
        });
        manifest
            .permissions
            .actions
            .insert("message.reply".to_owned());
        manifest.permissions.storage = StoragePermission::Private;
        manifest.permissions.storage_quota_bytes = 4;
        Self { manifest }
    }
}

#[async_trait]
impl StaticPlugin for QuotaFixturePlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        _queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        if message_text(event) != Some("/quota-fixture") {
            return Ok(HandlerOutput {
                disposition: Disposition::Ignore,
                ..HandlerOutput::default()
            });
        }
        Ok(HandlerOutput {
            disposition: Disposition::Continue,
            state_ops: vec![StateOp::Put {
                key: "oversized".to_owned(),
                value: b"more-than-four-bytes".to_vec(),
                expected_revision: None,
            }],
            commands: vec![PluginCommand {
                command_id: "must-not-run".to_owned(),
                kind: "message.reply".to_owned(),
                idempotency_key: Some(format!("{}/quota", event.event_id)),
                deadline_ms: None,
                payload: json!({"content":"quota command incorrectly executed"}),
            }],
            diagnostics: Vec::new(),
        })
    }
}

fn fixture_manifest(id: &str, name: &str, priority: i32, timeout_ms: u64) -> PluginManifest {
    PluginManifest {
        manifest_version: 1,
        id: PluginId::new(id).expect("fixture plugin ID must be valid"),
        metadata: PluginMetadata::single_locale("en", name, "Test-only plugin fixture"),
        version: "0.1.0".to_owned(),
        protocol: ">=1.0,<2.0".to_owned(),
        state_version: 1,
        entry: "component.wasm".to_owned(),
        runtime: PluginRuntimeConfig {
            timeout_ms,
            ..PluginRuntimeConfig::default()
        },
        subscriptions: vec![Subscription {
            id: "messages".to_owned(),
            event: "message.created".to_owned(),
            priority,
            scopes: BTreeSet::new(),
        }],
        commands: Vec::new(),
        permissions: PluginPermissions::default(),
    }
}

fn message_text(event: &PluginEventEnvelope) -> Option<&str> {
    event.payload.pointer("/data/text").and_then(Value::as_str)
}

pub fn fixture_configs() -> BTreeMap<String, BTreeMap<String, Value>> {
    BTreeMap::from([
        ("dev.bkm.timeout-fixture".to_owned(), BTreeMap::new()),
        ("dev.bkm.panic-fixture".to_owned(), BTreeMap::new()),
        ("dev.bkm.quota-fixture".to_owned(), BTreeMap::new()),
        ("dev.bkm.migration-fixture".to_owned(), BTreeMap::new()),
        ("dev.bkm.partition-fixture".to_owned(), BTreeMap::new()),
    ])
}
