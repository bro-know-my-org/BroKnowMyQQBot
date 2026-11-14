//! Bounded, memory-only local selections. Inspection never instantiates or installs a guest.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use plugin_host::ValidatedPluginPackage;
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::{ManagementError, PluginManager};

pub(crate) const MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;
const SELECTION_TTL: Duration = Duration::from_secs(300);
const MAX_SELECTIONS: usize = 2;

pub(super) struct Staging {
    selections: Mutex<BTreeMap<String, StagedPackage>>,
    slots: Arc<Semaphore>,
}

impl Default for Staging {
    fn default() -> Self {
        Self {
            selections: Mutex::default(),
            slots: Arc::new(Semaphore::new(MAX_SELECTIONS)),
        }
    }
}

pub(super) struct StagedPackage {
    _bytes: Vec<u8>,
    expires: Instant,
    _permit: OwnedSemaphorePermit,
}

#[derive(Serialize)]
pub(crate) struct LocalInspection {
    selection: String,
    expires_in_seconds: u64,
    plugin_id: String,
    name: String,
    description: String,
    version: String,
    sha256: String,
    signature_status: &'static str,
    commands: Vec<String>,
    requested_permissions: std::collections::BTreeSet<String>,
    default_instance_id: String,
}

impl PluginManager {
    pub(crate) fn discard_local_selection(&self, selection: &str) -> Result<(), ManagementError> {
        self.local_packages
            .selections
            .lock()
            .map_err(|_| ManagementError::Store)?
            .remove(selection);
        Ok(())
    }

    pub(crate) fn reserve_upload(&self) -> Result<OwnedSemaphorePermit, ManagementError> {
        self.local_packages
            .selections
            .lock()
            .map_err(|_| ManagementError::Store)?
            .retain(|_, selection| selection.expires > Instant::now());
        self.local_packages
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ManagementError::Busy)
    }

    pub(crate) async fn inspect_local_package(
        &self,
        bytes: Vec<u8>,
        permit: OwnedSemaphorePermit,
    ) -> Result<LocalInspection, ManagementError> {
        if bytes.len() > MAX_UPLOAD_BYTES {
            return Err(ManagementError::UploadTooLarge);
        }
        let manager = self.clone();
        // Supervision retains the upload permit until the blocking parser finishes,
        // even if the HTTP client disconnects while checking an archive.
        self.operation(async move {
            tokio::task::spawn_blocking(move || {
                let package = ValidatedPluginPackage::from_bytes(&bytes)
                    .map_err(|_| ManagementError::InvalidPackage)?;
                let manifest = package.manifest().clone();
                let metadata = manifest
                    .metadata
                    .resolve("zh-CN")
                    .map_err(|_| ManagementError::InvalidPackage)?;
                let selection = uuid::Uuid::new_v4().to_string();
                let result = LocalInspection {
                    selection: selection.clone(),
                    expires_in_seconds: SELECTION_TTL.as_secs(),
                    plugin_id: manifest.id.to_string(),
                    name: metadata.name.clone(),
                    description: metadata.description.clone(),
                    version: manifest.version.clone(),
                    sha256: package.package_sha256().to_owned(),
                    signature_status: "unsigned",
                    commands: manifest.commands.iter().map(|c| c.name.clone()).collect(),
                    requested_permissions: manifest.requested_capabilities(),
                    default_instance_id: format!("{}/default", manifest.id),
                };
                manager
                    .local_packages
                    .selections
                    .lock()
                    .map_err(|_| ManagementError::Store)?
                    .insert(
                        selection,
                        StagedPackage {
                            _bytes: bytes,
                            expires: Instant::now() + SELECTION_TTL,
                            _permit: permit,
                        },
                    );
                Ok(result)
            })
            .await
            .map_err(|_| ManagementError::Store)?
        })
        .await
    }
}

#[cfg(test)]
#[path = "local_packages_tests.rs"]
mod tests;
