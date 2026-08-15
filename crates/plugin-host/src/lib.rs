//! BKM plugin host implementation.

#![forbid(unsafe_code)]

mod http;
mod package;
mod runtime;
mod storage;
#[cfg(feature = "wasm")]
mod wasm;

pub(crate) use assets::AssetDigest;
pub use assets::{AssetError, AssetStore, StoredAsset};
pub use browser::{
    BrowserArtifact, BrowserExecution, BrowserExecutionError, BrowserExecutor,
    UnavailableBrowserExecutor,
};
pub use http::{HttpExecutionError, HttpExecutor, SecureHttpExecutor};
pub use package::{PluginPackageError, ValidatedPluginPackage};
pub use runtime::{PluginHostError, PluginInstanceState, StaticPluginHost, validate_plugin_config};
pub use storage::{
    CommitOptions, DeadLetter, OutboxOrigin, PluginInstallation, PluginStore, StoreError,
};
#[cfg(feature = "wasm")]
pub use wasm::{WasmPlugin, WasmPluginError};
mod assets;
mod browser;
