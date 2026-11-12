//! Functional probes that exist only to drive automated acceptance tests.

mod action_result;
mod config;

pub use action_result::ActionResultProbePlugin;
pub use config::ConfigProbePlugin;
