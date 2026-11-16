//! Bounded, memory-only local selections. Inspection never instantiates or installs a guest.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use plugin_host::ValidatedPluginPackage;
use serde::{Deserialize, Serialize};
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
    bytes: Vec<u8>,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalCommit {
    selection: String,
    sha256: String,
    plugin_id: String,
    version: String,
    instance_id: String,
    expected_updated_at_ms: Option<i64>,
    grants: std::collections::BTreeSet<String>,
    confirm_unsigned: bool,
    confirm: bool,
}

impl PluginManager {
    pub(crate) async fn commit_local_package(
        &self,
        request: LocalCommit,
    ) -> Result<super::marketplace_install::InstallResult, ManagementError> {
        if !request.confirm
            || !request.confirm_unsigned
            || uuid::Uuid::parse_str(&request.selection).is_err()
            || request.sha256.len() != 64
            || request.plugin_id.len() > 128
            || request.version.len() > 128
            || request.expected_updated_at_ms.is_some_and(|n| n < 0)
            || request.instance_id.is_empty()
            || request.instance_id.len() > 192
            || !request
                .instance_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
            || request
                .instance_id
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || request.grants.len() > 128
            || request.grants.iter().any(|p| p.len() > 256)
        {
            return Err(ManagementError::InvalidOperation);
        }
        let manager = self.clone();
        self.operation(async move { manager.commit_local_inner(request).await })
            .await
    }

    async fn commit_local_inner(
        &self,
        request: LocalCommit,
    ) -> Result<super::marketplace_install::InstallResult, ManagementError> {
        let staged = self
            .local_packages
            .selections
            .lock()
            .map_err(|_| ManagementError::Store)?
            .remove(&request.selection)
            .filter(|s| s.expires > Instant::now())
            .ok_or(ManagementError::ExpiredSelection)?;
        let StagedPackage {
            bytes,
            _permit: selection_permit,
            ..
        } = staged;
        let _permit = selection_permit;
        let (bytes, package) = tokio::task::spawn_blocking(move || {
            let package = ValidatedPluginPackage::from_bytes(&bytes)
                .map_err(|_| ManagementError::InvalidPackage)?;
            Ok::<_, ManagementError>((bytes, package))
        })
        .await
        .map_err(|_| ManagementError::Store)??;
        let manifest = package.manifest().clone();
        if package.package_sha256() != request.sha256
            || manifest.id.as_str() != request.plugin_id
            || manifest.version != request.version
        {
            return Err(ManagementError::InvalidPackage);
        }
        let id = &request.instance_id;
        let runtime = self.snapshot()?;
        if crate::plugins::is_reserved_bundled_instance_id(id)
            || self.file_instances.contains(id)
            || (runtime.host.instance_manifest(id).is_some() && !runtime.packages.contains_key(id))
        {
            return Err(ManagementError::NotManaged);
        }
        let store = self.store.clone();
        let instance_id = id.clone();
        let existing = tokio::task::spawn_blocking(move || store.installation(&instance_id))
            .await
            .map_err(|_| ManagementError::Store)??;
        if existing.as_ref().map(|i| i.updated_at_ms) != request.expected_updated_at_ms {
            return Err(ManagementError::Conflict);
        }
        if existing
            .as_ref()
            .is_some_and(|i| i.plugin_id != request.plugin_id)
        {
            return Err(ManagementError::NotManaged);
        }
        let requested = manifest.requested_capabilities();
        if !request.grants.is_subset(&requested) {
            return Err(ManagementError::InvalidOperation);
        }
        let mut granted: std::collections::BTreeSet<_> = existing
            .as_ref()
            .map(|i| {
                i.granted_permissions
                    .iter()
                    .filter(|p| requested.contains(*p))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        granted.extend(request.grants);
        let config = existing
            .as_ref()
            .map(|i| i.config.clone())
            .unwrap_or_default();
        let config_error = crate::plugin_installer::validate_install_candidate(package, &config)
            .await
            .map_err(|_| ManagementError::InvalidPackage)?;
        let pending = crate::plugin_installer::PendingInstallation {
            manifest,
            instance_id: request.instance_id,
            package_sha256: request.sha256,
            package_bytes: bytes,
            source: "browser-upload".to_owned(),
            permissions_complete: requested.is_subset(&granted),
            requested,
            granted,
            config,
            config_error,
            update: existing.is_some(),
        };
        self.commit_package(
            pending,
            existing,
            super::marketplace_install::InstallOrigin::Local,
        )
        .await
    }
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
                            bytes,
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
