//! Invocation snapshots for direct plugin unit tests.
use super::*;
use plugin_api::{CommandContext, StateValue};

pub(crate) struct CommandQueries<'a> {
    inner: &'a dyn HostQueries,
    command: Option<CommandContext>,
    prefix: String,
}

impl<'a> CommandQueries<'a> {
    pub(crate) fn new(event: &PluginEventEnvelope, inner: &'a dyn HostQueries) -> Self {
        Self::with_prefix(event, inner, &inner.command_prefix())
    }

    pub(crate) fn with_prefix(
        event: &PluginEventEnvelope,
        inner: &'a dyn HostQueries,
        prefix: &str,
    ) -> Self {
        let command = (event.event_type == "message.created")
            .then(|| event.payload.pointer("/data/text").and_then(Value::as_str))
            .flatten()
            .and_then(|text| CommandContext::parse(prefix, text));
        Self {
            inner,
            command,
            prefix: prefix.to_owned(),
        }
    }
}

impl HostQueries for CommandQueries<'_> {
    fn command_declarations(&self) -> Option<Vec<CommandDeclaration>> {
        self.inner.command_declarations()
    }
    fn command_context(&self) -> Option<CommandContext> {
        self.command.clone()
    }
    fn command_prefix(&self) -> String {
        self.prefix.clone()
    }
    fn config_get(&self, key: &str) -> Option<&Value> {
        self.inner.config_get(key)
    }
    fn state_get(&self, key: &str) -> Option<&StateValue> {
        self.inner.state_get(key)
    }
    fn state_scan(&self, prefix: &str, limit: usize) -> Vec<(&str, &StateValue)> {
        self.inner.state_scan(prefix, limit)
    }
    fn granted_capabilities(&self) -> &BTreeSet<String> {
        self.inner.granted_capabilities()
    }
    fn invocation_time_ms(&self) -> i64 {
        self.inner.invocation_time_ms()
    }
    fn caller_authority(&self) -> CallerAuthority {
        self.inner.caller_authority()
    }
}

#[async_trait]
pub(crate) trait TestPlugin: StaticPlugin {
    async fn on_test_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        self.on_event(event, &CommandQueries::new(event, queries))
            .await
    }
}
impl<T: StaticPlugin + ?Sized> TestPlugin for T {}
