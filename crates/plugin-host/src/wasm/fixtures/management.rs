// Build as a cdylib with wit-bindgen 0.46 and serde_json 1 on wasm32-unknown-unknown;
// componentize with wit-component, then base64 -w0 into management.component.b64.
wit_bindgen::generate!({path: "../../../../plugin-api/wit", world: "managed-plugin"});
use bkm::plugin::{queries, types::*};
use exports::bkm::plugin::{handler, lifecycle, management};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
static CALLS: AtomicU32 = AtomicU32::new(0);
struct Fixture;
impl lifecycle::Guest for Fixture {
    fn validate_config(_: Vec<ConfigEntry>) -> Result<(), PluginError> {
        panic!("no validate-config")
    }
    fn init(_: InitContext) -> Result<(), PluginError> {
        panic!("no init")
    }
    fn health() -> Result<HealthStatus, PluginError> {
        Ok(HealthStatus::Healthy)
    }
    fn migrate_state(_: u32, _: u32, _: Vec<StateEntry>) -> Result<MigrationOutput, PluginError> {
        panic!("no migrate")
    }
    fn shutdown() -> Result<(), PluginError> {
        panic!("no shutdown")
    }
}
impl handler::Guest for Fixture {
    fn on_event(_: EventEnvelope) -> Result<HandlerOutput, PluginError> {
        panic!("no business events")
    }
}
impl management::Guest for Fixture {
    fn invoke(request: EncodedValue) -> Result<management::Response, management::Error> {
        assert_eq!(
            CALLS.fetch_add(1, Ordering::Relaxed),
            0,
            "fresh guest for every management call"
        );
        assert!(matches!(queries::caller_authority().role, CallerRole::User));
        assert_eq!(queries::caller_authority().platform, "unknown");
        assert!(queries::caller_authority().subject_hint.is_none());
        assert!(queries::command_context().is_none());
        let request: serde_json::Value = serde_json::from_slice(&request.data).unwrap();
        match request["search"].as_str().unwrap() {
            "fuel" | "timeout" => loop {
                std::hint::black_box(());
            },
            "memory" => {
                let mut bytes = Vec::new();
                loop {
                    bytes.extend_from_slice(&[42; 65536]);
                    std::hint::black_box(&bytes);
                }
            }
            "error" => {
                return Err(management::Error {
                    code: management::ErrorCode::Conflict,
                    message: "conflict".into(),
                });
            }
            _ => {}
        }
        let config = queries::config_get("setting").unwrap().unwrap();
        let config: serde_json::Value = serde_json::from_slice(&config.data).unwrap();
        let state = queries::state_get("repos/A").unwrap();

        let mut records = Vec::new();
        let mut ops = Vec::new();
        if let Some(state) = state {
            records.push(json!({"id":"A","values":{"config":config,"revision":state.revision,"time":queries::invocation_time_ms()}}));
            let revision = if request["search"] == "row_conflict" {
                0
            } else {
                state.revision
            };
            if request["operation"] != "list" || request["search"] == "bad_list" {
                ops = vec![
                    StateOp::Put(PutState {
                        key: "repos/B".into(),
                        value: state.value,
                        expected_revision: None,
                    }),
                    StateOp::Delete(DeleteState {
                        key: "repos/A".into(),
                        expected_revision: Some(revision),
                    }),
                ];
            }
        }
        let mut data = json!({"records":records,"next_cursor":null});
        if request["search"] == "oversized" {
            data = json!({"records":[{"id":"A","values":{"text":"x".repeat(1024*1024)}}],"next_cursor":null});
        }
        if request["search"] == "many_ops" {
            ops = vec![
                StateOp::Delete(DeleteState {
                    key: "repos/A".into(),
                    expected_revision: None
                });
                257
            ];
        }
        Ok(management::Response {
            data: EncodedValue {
                schema_version: "1.0".into(),
                content_type: "application/json".into(),
                data: serde_json::to_vec(&data).unwrap(),
            },
            state_ops: ops,
        })
    }
}
export!(Fixture);
