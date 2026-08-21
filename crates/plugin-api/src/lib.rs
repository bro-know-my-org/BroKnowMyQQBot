//! BKM Plugin Protocol v1 types shared by static and WASM plugins.

#![forbid(unsafe_code)]

mod id;
mod manifest;
mod protocol;

pub use id::{PluginId, PluginIdError};
pub use manifest::{
    BrowserPermission, CommandDeclaration, HttpPermission, LocalizedPluginMetadata, ManifestError,
    PluginManifest, PluginMetadata, PluginPermissions, PluginRuntimeConfig, RuntimeMode,
    StoragePermission, Subscription, is_valid_http_credential_name, url_path_matches_prefix,
};
pub use protocol::{
    ActionCompleted, ActionStatus, AssetReference, BrowserColorScheme, BrowserRun,
    BrowserRunResult, BrowserScreenshotFormat, BrowserStep, BrowserViewport, BrowserWaitUntil,
    Disposition, ExtensionPayload, HandlerOutput, HealthStatus, HostQueries, HttpRequest,
    HttpResponse, InitContext, MediaReply, MediaSend, PluginCommand, PluginDiagnostic, PluginError,
    PluginEventEnvelope, PluginMessageTarget, ScheduleCancel, ScheduleCreate, ScheduleTriggered,
    StateOp, StateValue, StaticPlugin,
};

pub const BPP_VERSION: &str = "1.2.0";
