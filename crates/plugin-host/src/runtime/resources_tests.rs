use super::*;
use crate::{PluginStore, runtime::InvocationQueries};
use plugin_api::{
    CallerAuthority, HandlerOutput, ManagementDescriptor, ManagementOperation, PluginEventEnvelope,
    PluginManifest, StateOp,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

struct Probe {
    manifest: PluginManifest,
    descriptor: ManagementDescriptor,
    getter_panic: u8,
}
#[async_trait::async_trait]
impl StaticPlugin for Probe {
    fn manifest(&self) -> &PluginManifest {
        assert_ne!(self.getter_panic, 1, "manifest panic");
        &self.manifest
    }
    fn management_schema(&self) -> Option<&ManagementDescriptor> {
        assert_ne!(self.getter_panic, 2, "descriptor panic");
        Some(&self.descriptor)
    }
    async fn init(&self, _: plugin_api::InitContext) -> Result<(), plugin_api::PluginError> {
        panic!("management must not initialize");
    }
    async fn on_event(
        &self,
        _: &PluginEventEnvelope,
        _: &dyn HostQueries,
    ) -> Result<HandlerOutput, plugin_api::PluginError> {
        panic!("management must not invoke events");
    }
    async fn manage(
        &self,
        request: &ManagementRequest,
        queries: &dyn HostQueries,
    ) -> Result<ManagementResponse, ManagementError> {
        assert_eq!(queries.caller_authority(), CallerAuthority::default());
        assert!(queries.command_context().is_none());
        assert_eq!(queries.config_get("label"), Some(&json!("actual")));
        match request.search.as_str() {
            "panic" => panic!("payload must not escape"),
            "wait" => std::future::pending().await,
            _ => Ok(ManagementResponse {
                data: json!({"records":[],"next_cursor":null}),
                state_ops: if request.search == "bad-list" {
                    vec![StateOp::Put {
                        key: "bad".into(),
                        value: vec![],
                        expected_revision: None,
                    }]
                } else {
                    vec![]
                },
            }),
        }
    }
}

fn probe() -> Probe {
    let mut manifest = builtin_plugins::PingPlugin::default().manifest().clone();
    manifest.runtime.timeout_ms = 10;
    Probe { manifest, getter_panic:0, descriptor: serde_json::from_value(json!({"schema_version":1,"resources":[{"id":"aliases","title":"别名","schema":{"type":"object","properties":{"alias":{"type":"string"}},"required":["alias"],"additionalProperties":false},"columns":[{"key":"alias","title":"别名"}],"operations":["list","create"]}]})).unwrap() }
}
fn request(search: &str) -> ManagementRequest {
    ManagementRequest {
        resource: "aliases".into(),
        operation: ManagementOperation::List,
        search: search.into(),
        cursor: None,
        limit: 20,
        record_id: None,
        input: None,
    }
}

#[tokio::test]
async fn isolated_hook_has_no_init_identity_or_event_side_effects_and_is_supervised() {
    let plugin = probe();
    let config = BTreeMap::from([("label".into(), json!("actual"))]);
    let state = BTreeMap::new();
    let grants = BTreeSet::new();
    let authority = CallerAuthority::default();
    let queries = InvocationQueries {
        command_declarations: vec![],
        command_prefix: "/".into(),
        command_context: None,
        config: &config,
        state: &state,
        granted_capabilities: &grants,
        invocation_time_ms: 0,
        caller_authority: &authority,
    };
    assert!(
        invoke_management(&plugin, &request(""), &queries)
            .await
            .is_ok()
    );
    for getter_panic in [1, 2, 0] {
        let mut broken = probe();
        broken.getter_panic = getter_panic;
        broken.descriptor.schema_version = 0;
        assert_eq!(
            invoke_management(&broken, &request(""), &queries)
                .await
                .unwrap_err()
                .code,
            ManagementErrorCode::Internal
        );
    }
    for search in ["panic", "wait", "bad-list"] {
        let error = invoke_management(&plugin, &request(search), &queries)
            .await
            .unwrap_err();
        assert_eq!(error.code, ManagementErrorCode::Internal);
        assert!(!error.message.contains("payload"));
    }
    let mut invalid = request("");
    invalid.operation = ManagementOperation::Create;
    invalid.input = Some(json!({"alias":12}));
    assert_eq!(
        invoke_management(&plugin, &invalid, &queries)
            .await
            .unwrap_err()
            .code,
        ManagementErrorCode::InvalidArgument
    );
}

#[tokio::test]
async fn management_gate_waits_for_chat_and_blocks_invocations_until_commit() {
    let mut host = StaticPluginHost::new(PluginStore::in_memory().unwrap());
    host.register_trusted(
        Arc::new(builtin_plugins::PingPlugin::default()),
        "test",
        BTreeMap::new(),
    )
    .await
    .unwrap();
    let plugin = &host.plugins[0];
    let active_chat = plugin.execution.acquire("chat".into()).await.unwrap();
    let lock = host.lock_management_instance("test");
    tokio::pin!(lock);
    assert!(futures_util::poll!(&mut lock).is_pending());
    drop(active_chat);
    let lease = lock.await.unwrap().unwrap();
    assert_eq!(lease.snapshot.state, PluginInstanceState::Ready);
    let next_chat = plugin.execution.acquire("chat".into());
    tokio::pin!(next_chat);
    assert!(futures_util::poll!(&mut next_chat).is_pending());
    drop(lease);
    drop(next_chat.await.unwrap());
    assert_eq!(plugin.state().unwrap(), PluginInstanceState::Ready);
    for state in [PluginInstanceState::Stopped, PluginInstanceState::Disabled] {
        plugin.set_state(state).unwrap();
        let lease = host
            .lock_management_instance("test")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lease.snapshot.state, state);
        drop(lease);
        assert_eq!(plugin.state().unwrap(), state);
    }
    assert!(
        host.lock_management_instance("absent")
            .await
            .unwrap()
            .is_none()
    );
}
