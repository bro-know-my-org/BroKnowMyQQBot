//! BPP event, query, state, command, and lifecycle contracts.

use std::collections::{BTreeMap, BTreeSet};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{PluginId, PluginManifest};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginEventEnvelope {
    pub protocol_version: String,
    pub event_id: String,
    pub delivery_id: String,
    pub invocation_id: String,
    pub occurred_at_ms: Option<i64>,
    pub received_at_ms: i64,
    pub adapter_id: String,
    pub event_type: String,
    pub trace_id: Option<String>,
    pub payload: Value,
    #[serde(default)]
    pub extensions: Vec<ExtensionPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionPayload {
    pub namespace: String,
    pub schema_version: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    #[default]
    Continue,
    Stop,
    Ignore,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HandlerOutput {
    #[serde(default)]
    pub disposition: Disposition,
    #[serde(default)]
    pub state_ops: Vec<StateOp>,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum StateOp {
    Put {
        key: String,
        value: Vec<u8>,
        expected_revision: Option<u64>,
    },
    Delete {
        key: String,
        expected_revision: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginCommand {
    pub command_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Maximum execution duration, in milliseconds, once the Host starts this command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Vec<u8>>,
    #[serde(default = "default_http_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_http_response_limit")]
    pub max_response_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub final_url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BrowserRun {
    pub steps: Vec<BrowserStep>,
    #[serde(default)]
    pub viewport: BrowserViewport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_scheme: Option<BrowserColorScheme>,
    #[serde(default)]
    pub extra_headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserColorScheme {
    Light,
    Dark,
    NoPreference,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserViewport {
    pub width: u32,
    pub height: u32,
    #[serde(default = "default_device_scale_factor")]
    pub device_scale_factor: u8,
}

impl Default for BrowserViewport {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            device_scale_factor: 1,
        }
    }
}

const fn default_device_scale_factor() -> u8 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrowserStep {
    Navigate {
        url: String,
        #[serde(default)]
        wait_until: BrowserWaitUntil,
        #[serde(default = "default_browser_step_timeout_ms")]
        timeout_ms: u64,
    },
    Click {
        selector: String,
        #[serde(default = "default_browser_step_timeout_ms")]
        timeout_ms: u64,
    },
    Fill {
        selector: String,
        value: String,
        #[serde(default = "default_browser_step_timeout_ms")]
        timeout_ms: u64,
    },
    WaitFor {
        selector: String,
        #[serde(default = "default_browser_step_timeout_ms")]
        timeout_ms: u64,
    },
    WaitForIdle {
        #[serde(default = "default_browser_step_timeout_ms")]
        timeout_ms: u64,
    },
    Wait {
        duration_ms: u64,
    },
    ExtractText {
        selector: String,
        #[serde(default = "default_browser_step_timeout_ms")]
        timeout_ms: u64,
    },
    Screenshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
        #[serde(default)]
        full_page: bool,
        #[serde(default)]
        format: BrowserScreenshotFormat,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quality: Option<u8>,
    },
}

const fn default_browser_step_timeout_ms() -> u64 {
    10_000
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserWaitUntil {
    Load,
    #[default]
    DomContentLoaded,
    NetworkIdle,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BrowserScreenshotFormat {
    #[default]
    Png,
    Jpeg,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserRunResult {
    pub final_url: String,
    pub title: String,
    pub extracted_text: Vec<String>,
    pub assets: Vec<AssetReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetReference {
    pub asset_id: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub sha256: String,
    /// Informational wall-clock estimate; Host access control uses a monotonic TTL.
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaReply {
    pub asset_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(default = "default_consume_asset")]
    pub consume: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaSend {
    pub target: PluginMessageTarget,
    pub asset_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(default = "default_consume_asset")]
    pub consume: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum PluginMessageTarget {
    Group { group_id: String },
    Private { user_id: String },
    Channel { channel_id: String },
}

const fn default_consume_asset() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScheduleCreate {
    pub task_id: String,
    pub run_at_ms: i64,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleCancel {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScheduleTriggered {
    pub task_id: String,
    pub scheduled_at_ms: i64,
    pub payload: Value,
}

const fn default_http_timeout_ms() -> u64 {
    5_000
}

const fn default_http_response_limit() -> u64 {
    256 * 1024
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Succeeded,
    Failed,
    Denied,
    TimedOut,
    Unknown,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActionCompleted {
    pub source_event_id: String,
    pub source_invocation_id: String,
    pub command_id: String,
    pub kind: String,
    pub status: ActionStatus,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginDiagnostic {
    pub level: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateValue {
    pub value: Vec<u8>,
    pub revision: u64,
}

pub trait HostQueries: Send + Sync {
    fn config_get(&self, key: &str) -> Option<&Value>;
    fn state_get(&self, key: &str) -> Option<&StateValue>;
    fn state_scan(&self, prefix: &str, limit: usize) -> Vec<(&str, &StateValue)>;
    fn granted_capabilities(&self) -> &BTreeSet<String>;
    fn invocation_time_ms(&self) -> i64;
}

#[derive(Debug, Clone)]
pub struct InitContext {
    pub protocol_version: String,
    pub plugin_id: PluginId,
    pub instance_id: String,
    pub granted_capabilities: BTreeSet<String>,
    pub config: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Degraded,
}

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("invalid plugin configuration: {0}")]
    InvalidConfig(String),
    #[error("plugin permission denied: {0}")]
    PermissionDenied(String),
    #[error("plugin resource exhausted: {0}")]
    ResourceExhausted(String),
    #[error("plugin resource exhausted and guest trapped: {0}")]
    ResourceExhaustedTrap(String),
    #[error("plugin guest trapped: {0}")]
    GuestTrap(String),
    #[error("transient plugin error: {0}")]
    Transient(String),
    #[error("permanent plugin error: {0}")]
    Permanent(String),
}

#[async_trait]
pub trait StaticPlugin: Send + Sync + 'static {
    fn manifest(&self) -> &PluginManifest;

    /// Returns the optional JSON Schema used by the Host before `validate_config`.
    ///
    /// WASM plugins obtain the equivalent schema from `config.schema.json` in the
    /// plugin package. Static plugins expose it directly to preserve the same BPP
    /// configuration semantics without introducing package I/O.
    fn config_schema(&self) -> Option<&Value> {
        None
    }

    async fn validate_config(&self, _config: &BTreeMap<String, Value>) -> Result<(), PluginError> {
        Ok(())
    }

    async fn init(&self, _context: InitContext) -> Result<(), PluginError> {
        Ok(())
    }

    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        queries: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError>;

    async fn health(&self) -> Result<HealthStatus, PluginError> {
        Ok(HealthStatus::Healthy)
    }

    async fn migrate_state(
        &self,
        from_version: u32,
        to_version: u32,
        _state: &BTreeMap<String, StateValue>,
    ) -> Result<Vec<StateOp>, PluginError> {
        if from_version == to_version {
            Ok(Vec::new())
        } else {
            Err(PluginError::Permanent(format!(
                "no state migration from {from_version} to {to_version}"
            )))
        }
    }

    async fn shutdown(&self) -> Result<(), PluginError> {
        Ok(())
    }
}
