//! BKM Plugin Protocol v1 types shared by static and WASM plugins.

#![forbid(unsafe_code)]

mod id;
mod manifest;
mod protocol;

pub use id::{PluginId, PluginIdError};
pub use manifest::{
    CommandDeclaration, HttpPermission, LocalizedPluginMetadata, ManifestError, PluginManifest,
    PluginMetadata, PluginPermissions, PluginRuntimeConfig, RuntimeMode, StoragePermission,
    Subscription,
};
pub use protocol::{
    ActionCompleted, ActionStatus, Disposition, ExtensionPayload, HandlerOutput, HealthStatus,
    HostQueries, HttpRequest, HttpResponse, InitContext, PluginCommand, PluginDiagnostic,
    PluginError, PluginEventEnvelope, ScheduleCancel, ScheduleCreate, ScheduleTriggered, StateOp,
    StateValue, StaticPlugin,
};

pub const BPP_VERSION: &str = "1.0.0";
