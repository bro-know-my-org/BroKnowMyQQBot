use super::WasmPlugin;
use crate::ValidatedPluginPackage;
use base64::Engine as _;
use plugin_api::{
    CallerAuthority, CallerRole, CommandContext, HostQueries, ManagementErrorCode,
    ManagementOperation, ManagementRequest, StateOp, StateValue, StaticPlugin,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Cursor, Write},
    time::Duration,
};
use zip::{ZipWriter, write::SimpleFileOptions};

struct Queries {
    config: BTreeMap<String, Value>,
    state: BTreeMap<String, StateValue>,
    grants: BTreeSet<String>,
}
impl Default for Queries {
    fn default() -> Self {
        Self {
            config: BTreeMap::from([("setting".into(), json!("configured"))]),
            state: BTreeMap::from([(
                "repos/A".into(),
                StateValue {
                    value: b"owner/repo".to_vec(),
                    revision: 7,
                },
            )]),
            grants: BTreeSet::new(),
        }
    }
}
impl HostQueries for Queries {
    fn config_snapshot(&self) -> BTreeMap<String, Value> {
        self.config.clone()
    }
    fn config_get(&self, key: &str) -> Option<&Value> {
        self.config.get(key)
    }
    fn state_get(&self, key: &str) -> Option<&StateValue> {
        self.state.get(key)
    }
    fn state_scan(&self, prefix: &str, limit: usize) -> Vec<(&str, &StateValue)> {
        self.state
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .take(limit.min(4096))
            .map(|(k, v)| (k.as_str(), v))
            .collect()
    }
    fn granted_capabilities(&self) -> &BTreeSet<String> {
        &self.grants
    }
    fn invocation_time_ms(&self) -> i64 {
        12345
    }
    fn caller_authority(&self) -> CallerAuthority {
        CallerAuthority {
            role: CallerRole::Owner,
            platform: "qq.official".into(),
            subject_hint: Some("owner".into()),
        }
    }
    fn command_context(&self) -> Option<CommandContext> {
        Some(CommandContext {
            prefix: "/".into(),
            name: "danger".into(),
            arguments: String::new(),
        })
    }
}
fn request(operation: ManagementOperation, search: &str) -> ManagementRequest {
    ManagementRequest {
        resource: "repos".into(),
        operation,
        search: search.into(),
        cursor: None,
        limit: 20,
        record_id: None,
        input: None,
    }
}
async fn plugin(fuel: u64, timeout: u64, memory: u32, legacy: bool) -> WasmPlugin {
    let manifest = format!(
        r#"manifest_version=1
id="dev.bkm.management-fixture"
version="0.1.0"
protocol=">=1.0,<2"
[metadata]
default_locale="en"
[metadata.locales.en]
name="Management Fixture"
[runtime]
fuel={fuel}
timeout_ms={timeout}
memory_mb={memory}
"#
    );
    let encoded = if legacy {
        // Frozen core plugin world 1.5, generated from the existing ping fixture source.
        include_str!("fixtures/legacy15.component.b64")
    } else {
        include_str!("fixtures/management.component.b64")
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .unwrap();
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    for (name,data) in [("plugin.toml",manifest.as_bytes()),("component.wasm",&bytes),("management.schema.json",br#"{"schema_version":1,"resources":[{"id":"repos","title":"Repos","schema":{"type":"object"},"columns":[],"operations":["list","create","update","delete"]}]}"#.as_slice())] {
        zip.start_file(name,SimpleFileOptions::default()).unwrap();zip.write_all(data).unwrap();
    }
    WasmPlugin::from_package(
        ValidatedPluginPackage::from_bytes(&zip.finish().unwrap().into_inner()).unwrap(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn detached_manage_preloads_config_state_without_init_or_chat_identity() {
    let plugin = plugin(10_000_000, 3000, 32, false).await;
    assert_eq!(plugin.management_schema().unwrap().resources[0].id, "repos");
    let mut queries = Queries::default();
    for setting in ["first", "second"] {
        queries.config.insert("setting".into(), json!(setting));
        let response = plugin
            .manage(&request(ManagementOperation::Update, ""), &queries)
            .await
            .unwrap();
        assert_eq!(
            response.data["records"][0]["values"],
            json!({"config":setting,"revision":7,"time":12345})
        );
        assert!(
            matches!(&response.state_ops[0],StateOp::Put{key,value,expected_revision:None} if key=="repos/B" && value==b"owner/repo")
        );
        assert!(matches!(
            &response.state_ops[1],
            StateOp::Delete {
                expected_revision: Some(7),
                ..
            }
        ));
    }
    assert_eq!(
        plugin
            .manage(&request(ManagementOperation::List, ""), &queries)
            .await
            .unwrap()
            .state_ops,
        Vec::<StateOp>::new()
    );
    let conflict = plugin
        .manage(
            &request(ManagementOperation::Delete, "row_conflict"),
            &queries,
        )
        .await
        .unwrap();
    assert!(matches!(
        &conflict.state_ops[1],
        StateOp::Delete {
            expected_revision: Some(0),
            ..
        }
    ));
    queries.state.clear();
    assert_eq!(
        plugin
            .manage(&request(ManagementOperation::List, ""), &queries)
            .await
            .unwrap()
            .data["records"],
        json!([])
    );
}

#[tokio::test]
async fn optional_hook_and_business_errors_remain_typed() {
    let legacy = plugin(10_000_000, 3000, 32, true).await;
    assert_eq!(
        legacy
            .manage(&request(ManagementOperation::List, ""), &Queries::default())
            .await
            .unwrap_err()
            .code,
        ManagementErrorCode::Unsupported
    );
    let plugin = plugin(10_000_000, 3000, 32, false).await;
    let error = plugin
        .manage(
            &request(ManagementOperation::List, "error"),
            &Queries::default(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ManagementErrorCode::Conflict);
    assert_eq!(error.message, "conflict");
    assert_eq!(
        plugin
            .manage(
                &request(ManagementOperation::List, "bad_list"),
                &Queries::default()
            )
            .await
            .unwrap_err()
            .code,
        ManagementErrorCode::Internal
    );
}

#[tokio::test]
async fn management_limits_are_enforced_and_failures_do_not_poison_next_call() {
    for (case, fuel, timeout, memory) in [
        ("fuel", 50_000, 3000, 32),
        ("timeout", 1_000_000_000_000, 10, 32),
        ("memory", 1_000_000_000, 3000, 8),
        ("oversized", 1_000_000_000, 3000, 32),
        ("many_ops", 10_000_000, 3000, 32),
    ] {
        let plugin = plugin(fuel, timeout, memory, false).await;
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            plugin.manage(
                &request(ManagementOperation::Update, case),
                &Queries::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.code, ManagementErrorCode::Internal, "{case}");
        {
            plugin
                .manage(&request(ManagementOperation::List, ""), &Queries::default())
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn detached_calls_wait_for_the_existing_engine_invocation_and_epoch() {
    let plugin = std::sync::Arc::new(plugin(1_000_000_000, 100, 32, false).await);
    let execution = plugin.lock_instance().await;
    let pending = {
        let plugin = plugin.clone();
        tokio::spawn(async move {
            plugin
                .manage(&request(ManagementOperation::List, ""), &Queries::default())
                .await
        })
    };
    tokio::task::yield_now().await;
    assert!(!pending.is_finished());
    // Simulate the currently running business invocation reaching its deadline.
    plugin.engine.increment_epoch();
    drop(execution);
    pending.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_management_deadlines_cannot_interrupt_recovery() {
    let plugin = std::sync::Arc::new(plugin(1_000_000_000_000, 30, 32, false).await);
    for _ in 0..10 {
        let pending = {
            let plugin = plugin.clone();
            tokio::spawn(async move {
                plugin
                    .manage(
                        &request(ManagementOperation::List, "timeout"),
                        &Queries::default(),
                    )
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(25)).await;
        pending.abort();
        let _ = pending.await;
        plugin
            .manage(&request(ManagementOperation::List, ""), &Queries::default())
            .await
            .unwrap();
    }
}
