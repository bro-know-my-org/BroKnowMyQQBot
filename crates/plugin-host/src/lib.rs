//! BKM plugin host implementation.

#![forbid(unsafe_code)]

mod http;
mod package;
mod runtime;
mod storage;
#[cfg(feature = "wasm")]
mod wasm;

pub use http::{HttpExecutionError, HttpExecutor, SecureHttpExecutor};
pub use package::{PluginPackageError, ValidatedPluginPackage};
pub use runtime::{PluginHostError, PluginInstanceState, StaticPluginHost};
pub use storage::{PluginStore, StoreError};
#[cfg(feature = "wasm")]
pub use wasm::{WasmPlugin, WasmPluginError};
