wit_bindgen::generate!({
    path: "../../../crates/plugin-api/wit",
    world: "plugin",
});

use bkm::plugin::types::{
    Command, CommandPayload, Disposition, EventEnvelope, EventPayload, HandlerOutput, HealthStatus,
    InitContext, MessageReplyCommand, MigrationOutput, PluginError, StateEntry,
};
use exports::bkm::plugin::{handler::Guest as HandlerGuest, lifecycle::Guest as LifecycleGuest};

struct WasmPing;

impl LifecycleGuest for WasmPing {
    fn validate_config(_config: Vec<bkm::plugin::types::ConfigEntry>) -> Result<(), PluginError> {
        Ok(())
    }

    fn init(_context: InitContext) -> Result<(), PluginError> {
        Ok(())
    }

    fn health() -> Result<HealthStatus, PluginError> {
        Ok(HealthStatus::Healthy)
    }

    fn migrate_state(
        _from_version: u32,
        _to_version: u32,
        _state: Vec<StateEntry>,
    ) -> Result<MigrationOutput, PluginError> {
        Ok(MigrationOutput {
            state_ops: Vec::new(),
        })
    }

    fn shutdown() -> Result<(), PluginError> {
        Ok(())
    }
}

impl HandlerGuest for WasmPing {
    fn on_event(event: EventEnvelope) -> Result<HandlerOutput, PluginError> {
        let EventPayload::MessageCreated(message) = event.payload else {
            return Ok(ignored());
        };
        match message.text.trim() {
            "/ping" => Ok(reply(&event.event_id, "pong".to_owned())),
            "/wasm-trap" => panic!("intentional WASM fixture trap"),
            "/wasm-fuel" | "/wasm-timeout" => loop {
                std::hint::black_box(());
            },
            "/wasm-memory" => {
                let allocation = vec![0xA5_u8; 64 * 1024 * 1024];
                std::hint::black_box(allocation);
                Ok(ignored())
            }
            "/wasm-output" => Ok(reply(&event.event_id, "x".repeat(2 * 1024 * 1024))),
            _ => Ok(ignored()),
        }
    }
}

fn reply(event_id: &str, content: String) -> HandlerOutput {
    HandlerOutput {
        disposition: Disposition::Continue,
        state_ops: Vec::new(),
        commands: vec![Command {
            command_id: "reply".to_owned(),
            idempotency_key: Some(format!("{event_id}/reply")),
            deadline_ms: Some(10_000),
            payload: CommandPayload::MessageReply(MessageReplyCommand { content }),
        }],
        diagnostics: Vec::new(),
    }
}

fn ignored() -> HandlerOutput {
    HandlerOutput {
        disposition: Disposition::Ignore,
        state_ops: Vec::new(),
        commands: Vec::new(),
        diagnostics: Vec::new(),
    }
}

export!(WasmPing);
