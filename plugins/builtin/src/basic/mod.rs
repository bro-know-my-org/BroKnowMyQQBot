//! Core user-facing bundled plugins.

use super::{
    CommandDeclaration, Disposition, HandlerOutput, HostQueries, PluginError, PluginEventEnvelope,
    PluginManifest, StateOp, StaticPlugin, async_trait, command_argument, command_declaration,
    ignored, is_command, message_plugin_manifest, message_text, reply,
};

mod counter;
mod echo;
mod help;
mod ping;

pub use counter::CounterPlugin;
pub use echo::EchoPlugin;
pub use help::HelpPlugin;
pub use ping::PingPlugin;
