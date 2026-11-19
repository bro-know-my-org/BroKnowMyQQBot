//! BKM Plugin Protocol v1 types shared by static and WASM plugins.

#![forbid(unsafe_code)]

mod command;
mod id;
mod management;
mod manifest;
mod protocol;
mod qq;

pub use command::{
    CommandContext, DEFAULT_COMMAND_PREFIX, InvalidCommandPrefix, MAX_COMMAND_PREFIX_CHARS,
    validate_command_prefix,
};
pub use id::{PluginId, PluginIdError};
pub use management::*;
pub use manifest::{
    Audience, BrowserPermission, CommandDeclaration, HttpPermission, LocalizedPluginMetadata,
    ManifestError, PluginManifest, PluginMetadata, PluginPermissions, PluginRuntimeConfig,
    RuntimeMode, StoragePermission, SubcommandDeclaration, Subscription,
    is_valid_http_credential_name, url_path_matches_prefix,
};
pub use protocol::{
    ActionCompleted, ActionStatus, AssetReference, BrowserColorScheme, BrowserRun,
    BrowserRunResult, BrowserScreenshotFormat, BrowserStep, BrowserViewport, BrowserWaitUntil,
    CallerAuthority, CallerRole, Disposition, ExtensionPayload, HandlerOutput, HealthStatus,
    HostQueries, HttpRequest, HttpResponse, InitContext, MediaCreate, MediaReply, MediaSend,
    PluginCommand, PluginDiagnostic, PluginError, PluginEventEnvelope, PluginMessageTarget,
    ScheduleCancel, ScheduleCreate, ScheduleTriggered, StateOp, StateValue, StaticPlugin,
};

pub use qq::*;

pub const BPP_VERSION: &str = "1.7.0";
