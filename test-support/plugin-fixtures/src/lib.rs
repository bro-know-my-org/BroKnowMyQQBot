//! BPP plugins used only by automated Host acceptance tests.

#![forbid(unsafe_code)]

mod faults;
mod probes;

pub use faults::{
    MigrationFixturePlugin, PanicFixturePlugin, PartitionFixturePlugin, PartitionStats,
    QuotaFixturePlugin, TimeoutFixturePlugin,
};
pub use probes::{ActionResultProbePlugin, ConfigProbePlugin};
