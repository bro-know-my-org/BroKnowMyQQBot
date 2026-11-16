//! A marketplace update must advance saved intent without downgrading the running guest.
use super::{ManagementError, PluginManager};
use plugin_host::PluginInstallation;
use semver::Version;

impl PluginManager {
    pub(super) fn check_marketplace_version(
        &self,
        candidate: &str,
        saved: Option<&PluginInstallation>,
        instance_id: &str,
    ) -> Result<(), ManagementError> {
        let runtime = self.snapshot()?;
        let loaded = runtime.host.instance_manifest(instance_id);
        require_upgrade(
            candidate,
            saved.map(|saved| saved.version.as_str()),
            loaded.as_ref().map(|p| p.version.as_str()),
        )
    }
}

fn require_upgrade(
    candidate: &str,
    saved: Option<&str>,
    loaded: Option<&str>,
) -> Result<(), ManagementError> {
    let parse =
        |value: &str| Version::parse(value).map_err(|_| ManagementError::MarketplaceVersion);
    let candidate = parse(candidate)?;
    let saved = saved.map(parse).transpose()?;
    if saved.is_some_and(|saved| !candidate.cmp_precedence(&saved).is_gt())
        || loaded
            .map(parse)
            .transpose()?
            .is_some_and(|loaded| candidate.cmp_precedence(&loaded).is_lt())
    {
        return Err(ManagementError::MarketplaceVersion);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::require_upgrade;

    #[test]
    fn marketplace_update_uses_semver_precedence_and_checks_both_saved_and_loaded() {
        for (candidate, saved, loaded) in [
            ("0.2.6", "0.4.0", None),
            ("0.4.0", "0.4.0", None),
            ("1.0.0+new", "1.0.0+old", None),
            ("1.0.0-rc.1", "1.0.0", None),
            ("0.3.0", "0.2.6", Some("0.4.0")),
            ("v1.0.0", "0.4.0", None),
            ("1.0.0", "invalid", None),
            ("1.0.0", "0.4.0", Some("invalid")),
            ("18446744073709551616.0.0", "0.4.0", None),
        ] {
            assert!(
                require_upgrade(candidate, Some(saved), loaded).is_err(),
                "{candidate} {saved} {loaded:?}"
            );
        }
        for (candidate, saved, loaded) in [
            ("0.4.0", "0.2.6", None),
            ("0.10.0", "0.9.0", Some("0.9.0")),
            ("1.0.0", "1.0.0-rc.9", None),
            ("1.0.0-rc.10", "1.0.0-rc.9", None),
            ("0.4.0", "0.2.6", Some("0.4.0")),
            (
                "1.0.0-9999999999999999999999999999999",
                "1.0.0-9999999999999999999999999999998",
                None,
            ),
        ] {
            assert!(
                require_upgrade(candidate, Some(saved), loaded).is_ok(),
                "{candidate} {saved} {loaded:?}"
            );
        }
        assert!(require_upgrade("0.2.6", None, Some("0.4.0")).is_err());
        assert!(require_upgrade("0.4.0", None, None).is_ok());
    }

    struct Guest(plugin_api::PluginManifest);
    #[async_trait::async_trait]
    impl plugin_api::StaticPlugin for Guest {
        fn manifest(&self) -> &plugin_api::PluginManifest {
            &self.0
        }
        async fn on_event(
            &self,
            _: &plugin_api::PluginEventEnvelope,
            _: &dyn plugin_api::HostQueries,
        ) -> Result<plugin_api::HandlerOutput, plugin_api::PluginError> {
            Ok(plugin_api::HandlerOutput::default())
        }
    }

    #[tokio::test]
    async fn marketplace_commit_rechecks_loaded_version_after_waiting_for_the_write_lock() {
        use crate::{
            plugin_configuration::fixture::Fixture,
            plugin_installer::PendingInstallation,
            plugin_management::{
                ManagementError, PluginManager, marketplace_install::InstallOrigin,
            },
        };
        use std::{
            collections::{BTreeMap, BTreeSet},
            sync::Arc,
        };
        let fixture = Fixture::new(true);
        let package = plugin_host::ValidatedPluginPackage::from_path(std::path::Path::new(
            &fixture.installation.package_path,
        ))
        .unwrap();
        let mut candidate = package.manifest().clone();
        candidate.version = "0.3.0".into();
        let mut saved = fixture.installation.clone();
        saved.version = "0.2.0".into();
        fixture.store.upsert_installation(&saved).unwrap();
        let manager = PluginManager::new(
            plugin_host::StaticPluginHost::new(fixture.store.clone()),
            fixture.store.clone(),
            BTreeMap::new(),
            BTreeSet::new(),
        )
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            None,
            crate::plugin_marketplace::DEFAULT_MARKETPLACE_URL.into(),
        )
        .unwrap();
        manager
            .check_marketplace_version("0.3.0", Some(&saved), &saved.instance_id)
            .unwrap();
        let mut loaded = candidate.clone();
        loaded.version = "0.4.0".into();
        let mut host = plugin_host::StaticPluginHost::new(fixture.store.clone());
        host.register(
            Arc::new(Guest(loaded)),
            &saved.instance_id,
            BTreeMap::new(),
            saved.granted_permissions.iter().cloned().collect(),
        )
        .await
        .unwrap();
        let pending = PendingInstallation {
            manifest: candidate,
            instance_id: saved.instance_id.clone(),
            package_sha256: "0".repeat(64),
            package_bytes: Vec::new(),
            source: "test".into(),
            requested: BTreeSet::new(),
            granted: BTreeSet::new(),
            config: BTreeMap::new(),
            permissions_complete: true,
            config_error: None,
            update: true,
        };
        let write = manager.hold_writes_for_test().await;
        let mut operation = Box::pin(manager.commit_package(
            pending,
            Some(saved.clone()),
            InstallOrigin::Marketplace,
        ));
        assert!(futures_util::poll!(operation.as_mut()).is_pending());
        manager.runtime.write().unwrap().host = host;
        drop(write);
        assert!(matches!(
            operation.await,
            Err(ManagementError::MarketplaceVersion)
        ));
        assert_eq!(
            fixture
                .store
                .installation(&saved.instance_id)
                .unwrap()
                .unwrap(),
            saved
        );
        assert!(!fixture.directory.join("plugins").exists());
        manager.shutdown().await.unwrap();
    }
}
