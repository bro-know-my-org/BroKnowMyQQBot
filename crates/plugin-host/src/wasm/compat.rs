//! Preserve the pre-1.5 handler result ABI: Component variants require exact cases.

use super::conversion::{guest_error, handler_output, runtime_error, wit_event};
use super::{StoreData, exports, qq, types};
use plugin_api::{HandlerOutput, PluginError, PluginEventEnvelope};
use wasmtime::{
    Store,
    component::{ComponentType, Instance, Lift, TypedFunc},
};

#[derive(ComponentType, Lift)]
#[component(variant)]
enum LegacyPayload {
    #[component(name = "message-reply")]
    MessageReply(types::MessageReplyCommand),
    #[component(name = "message-send")]
    MessageSend(types::MessageSendCommand),
    #[component(name = "media-reply")]
    MediaReply(types::MediaReplyCommand),
    #[component(name = "media-send")]
    MediaSend(types::MediaSendCommand),
    #[component(name = "http-request")]
    HttpRequest(types::HttpRequestCommand),
    #[component(name = "browser-run")]
    BrowserRun(types::BrowserRunCommand),
    #[component(name = "schedule-create")]
    ScheduleCreate(types::ScheduleCreateCommand),
    #[component(name = "schedule-cancel")]
    ScheduleCancel(types::ScheduleCancelCommand),
}

#[derive(ComponentType, Lift)]
#[component(record)]
struct LegacyCommand {
    #[component(name = "command-id")]
    command_id: String,
    #[component(name = "idempotency-key")]
    idempotency_key: Option<String>,
    #[component(name = "deadline-ms")]
    deadline_ms: Option<u64>,
    payload: LegacyPayload,
}

#[derive(ComponentType, Lift)]
#[component(record)]
struct LegacyOutput {
    disposition: types::Disposition,
    #[component(name = "state-ops")]
    state_ops: Vec<types::StateOp>,
    commands: Vec<LegacyCommand>,
    diagnostics: Vec<types::Diagnostic>,
}

type LegacyHandler =
    TypedFunc<(types::EventEnvelope,), (Result<LegacyOutput, types::PluginError>,)>;

pub(super) struct Bindings {
    lifecycle: exports::bkm::plugin::lifecycle::Guest,
    handler: Handler,
    pub(super) management: Option<exports::bkm::plugin::management::Guest>,
}

enum Handler {
    Qq(exports::bkm::plugin::qq_handler::Guest),
    Current(exports::bkm::plugin::handler::Guest),
    Legacy(LegacyHandler),
}

impl Bindings {
    pub(super) fn new(
        store: &mut Store<StoreData>,
        instance: &Instance,
        requires_qq: bool,
    ) -> anyhow::Result<Self> {
        let pre = instance.instance_pre(&*store);
        let lifecycle = exports::bkm::plugin::lifecycle::GuestIndices::new(&pre)?
            .load(&mut *store, instance)?;
        let has_qq = pre
            .component()
            .get_export_index(None, "bkm:plugin/qq-handler@1.7.0")
            .is_some();
        let handler = if has_qq || requires_qq {
            anyhow::ensure!(
                has_qq,
                "BPP QQ Markdown and buttons require the qq-handler export"
            );
            Handler::Qq(
                exports::bkm::plugin::qq_handler::GuestIndices::new(&pre)?
                    .load(&mut *store, instance)?,
            )
        } else {
            match exports::bkm::plugin::handler::GuestIndices::new(&pre)?
                .load(&mut *store, instance)
            {
                Ok(handler) => Handler::Current(handler),
                Err(current_error) => {
                    let interface = pre
                        .component()
                        .get_export_index(None, "bkm:plugin/handler@1.5.0")
                        .ok_or_else(|| anyhow::anyhow!("missing BPP handler"))?;
                    let function = pre
                        .component()
                        .get_export_index(Some(&interface), "on-event")
                        .ok_or_else(|| anyhow::anyhow!("missing BPP on-event"))?;
                    let function = instance.get_typed_func(&mut *store, function)
                    .map_err(|legacy_error| anyhow::anyhow!(
                        "unsupported BPP handler ABI: {current_error}; legacy: {legacy_error}"
                    ))?;
                    Handler::Legacy(function)
                }
            }
        };
        let management = if pre
            .component()
            .get_export_index(None, "bkm:plugin/management@1.6.0")
            .is_some()
        {
            Some(
                exports::bkm::plugin::management::GuestIndices::new(&pre)?
                    .load(&mut *store, instance)?,
            )
        } else {
            None
        };
        Ok(Self {
            lifecycle,
            handler,
            management,
        })
    }

    pub(super) fn bkm_plugin_lifecycle(&self) -> &exports::bkm::plugin::lifecycle::Guest {
        &self.lifecycle
    }

    pub(super) async fn call_on_event(
        &self,
        store: &mut Store<StoreData>,
        event: &PluginEventEnvelope,
    ) -> Result<HandlerOutput, PluginError> {
        if let Handler::Qq(handler) = &self.handler {
            let output = handler
                .call_on_event(store, &qq::wit_event(event)?)
                .await
                .map_err(runtime_error)?
                .map_err(guest_error)?;
            return qq::handler_output(output, event.event_type == plugin_api::QQ_BUTTON_CLICKED);
        }
        let output = self
            .call_core_event(store, &wit_event(event)?)
            .await
            .map_err(runtime_error)?
            .map_err(guest_error)?;
        handler_output(output)
    }

    async fn call_core_event(
        &self,
        store: &mut Store<StoreData>,
        event: &types::EventEnvelope,
    ) -> anyhow::Result<Result<types::HandlerOutput, types::PluginError>> {
        match &self.handler {
            Handler::Qq(_) => unreachable!("QQ events use the QQ handler directly"),
            Handler::Current(handler) => handler.call_on_event(store, event).await,
            Handler::Legacy(handler) => {
                let (output,) = handler.call_async(&mut *store, (event.clone(),)).await?;
                handler.post_return_async(store).await?;
                Ok(output.map(|output| types::HandlerOutput {
                    disposition: output.disposition,
                    state_ops: output.state_ops,
                    diagnostics: output.diagnostics,
                    commands: output
                        .commands
                        .into_iter()
                        .map(|command| types::Command {
                            command_id: command.command_id,
                            idempotency_key: command.idempotency_key,
                            deadline_ms: command.deadline_ms,
                            payload: match command.payload {
                                LegacyPayload::MessageReply(v) => {
                                    types::CommandPayload::MessageReply(v)
                                }
                                LegacyPayload::MessageSend(v) => {
                                    types::CommandPayload::MessageSend(v)
                                }
                                LegacyPayload::MediaReply(v) => {
                                    types::CommandPayload::MediaReply(v)
                                }
                                LegacyPayload::MediaSend(v) => types::CommandPayload::MediaSend(v),
                                LegacyPayload::HttpRequest(v) => {
                                    types::CommandPayload::HttpRequest(v)
                                }
                                LegacyPayload::BrowserRun(v) => {
                                    types::CommandPayload::BrowserRun(v)
                                }
                                LegacyPayload::ScheduleCreate(v) => {
                                    types::CommandPayload::ScheduleCreate(v)
                                }
                                LegacyPayload::ScheduleCancel(v) => {
                                    types::CommandPayload::ScheduleCancel(v)
                                }
                            },
                        })
                        .collect(),
                }))
            }
        }
    }
}
