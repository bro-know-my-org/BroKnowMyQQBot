//! Validated, version-checked installation edits shared by CLI and Web management.

use plugin_api::StaticPlugin;
use plugin_host::{
    PluginInstallation, PluginStore, StoreError, ValidatedPluginPackage, WasmPlugin,
    validate_plugin_config,
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
use thiserror::Error;

#[derive(Debug, Error)]
pub(crate) enum ConfigurationError {
    #[error("插件不存在，或由声明式文件管理")]
    NotManaged,
    #[error("插件记录已改变，请刷新后重试")]
    Conflict,
    #[error("插件包校验失败，请检查安装记录或重新安装")]
    InvalidPackage,
    #[error("配置校验失败，请检查字段类型、必填项和插件配置约束")]
    InvalidConfig,
    #[error("插件缺少必要授权，请通过安装命令显式授权")]
    MissingGrants,
    #[error("插件管理存储不可用")]
    Store,
}

impl From<StoreError> for ConfigurationError {
    fn from(error: StoreError) -> Self {
        if matches!(error, StoreError::InstallationConflict) {
            Self::Conflict
        } else {
            Self::Store
        }
    }
}

pub(crate) async fn current(
    store: &PluginStore,
    instance_id: &str,
    expected: i64,
) -> Result<PluginInstallation, ConfigurationError> {
    if crate::plugins::is_reserved_bundled_instance_id(instance_id) {
        return Err(ConfigurationError::NotManaged);
    }
    let store = store.clone();
    let instance_id = instance_id.to_owned();
    let installation = tokio::task::spawn_blocking(move || store.installation(&instance_id))
        .await
        .map_err(|_| ConfigurationError::Store)??
        .ok_or(ConfigurationError::NotManaged)?;
    if installation.updated_at_ms != expected {
        return Err(ConfigurationError::Conflict);
    }
    Ok(installation)
}

pub(crate) async fn save_config(
    store: &PluginStore,
    instance_id: &str,
    expected: i64,
    config: BTreeMap<String, Value>,
) -> Result<(), ConfigurationError> {
    let mut installation = current(store, instance_id, expected).await?;
    if serde_json::to_vec(&config)
        .map_err(|_| ConfigurationError::InvalidConfig)?
        .len()
        > 256 * 1024
    {
        return Err(ConfigurationError::InvalidConfig);
    }
    let plugin = checked_plugin(&installation).await?;
    validate_plugin_config(plugin.as_ref(), &config)
        .await
        .map_err(|_| ConfigurationError::InvalidConfig)?;
    installation.config = config;
    write_record(store, installation, expected).await
}

pub(crate) async fn save_enabled(
    store: &PluginStore,
    instance_id: &str,
    expected: i64,
    enabled: bool,
) -> Result<(), ConfigurationError> {
    let mut installation = current(store, instance_id, expected).await?;
    if enabled {
        require_grants(&installation)?;
        let plugin = checked_plugin(&installation).await?;
        validate_plugin_config(plugin.as_ref(), &installation.config)
            .await
            .map_err(|_| ConfigurationError::InvalidConfig)?;
    }
    installation.enabled = enabled;
    write_record(store, installation, expected).await
}

pub(crate) async fn checked_package(
    item: &PluginInstallation,
) -> Result<ValidatedPluginPackage, ConfigurationError> {
    let item = item.clone();
    tokio::task::spawn_blocking(move || {
        let package = ValidatedPluginPackage::from_path(std::path::Path::new(&item.package_path))
            .map_err(|_| ConfigurationError::InvalidPackage)?;
        if package.package_sha256() != item.package_sha256
            || package.manifest().id.as_str() != item.plugin_id
            || package.manifest().version != item.version
        {
            return Err(ConfigurationError::InvalidPackage);
        }
        Ok(package)
    })
    .await
    .map_err(|_| ConfigurationError::Store)?
}

pub(crate) async fn checked_plugin(
    item: &PluginInstallation,
) -> Result<Arc<dyn StaticPlugin>, ConfigurationError> {
    let plugin = WasmPlugin::from_package(checked_package(item).await?)
        .await
        .map_err(|_| ConfigurationError::InvalidPackage)?;
    Ok(Arc::new(plugin))
}

pub(crate) fn require_grants(item: &PluginInstallation) -> Result<(), ConfigurationError> {
    if item
        .requested_permissions
        .iter()
        .all(|permission| item.granted_permissions.contains(permission))
    {
        Ok(())
    } else {
        Err(ConfigurationError::MissingGrants)
    }
}

async fn write_record(
    store: &PluginStore,
    mut item: PluginInstallation,
    expected: i64,
) -> Result<(), ConfigurationError> {
    item.updated_at_ms = chrono::Utc::now().timestamp_millis().max(
        expected
            .checked_add(1)
            .ok_or(ConfigurationError::Conflict)?,
    );
    let store = store.clone();
    tokio::task::spawn_blocking(move || store.write_installation(&item, Some(expected)))
        .await
        .map_err(|_| ConfigurationError::Store)??;
    Ok(())
}

#[cfg(test)]
pub(crate) mod fixture;

#[cfg(test)]
mod tests {
    use super::*;
    use fixture::Fixture;
    use serde_json::json;

    #[tokio::test]
    async fn configuration_edits_validate_and_use_compare_and_swap_without_enabling_plugins() {
        let fixture = Fixture::new(false);
        let id = &fixture.installation.instance_id;
        let config = BTreeMap::from([("greeting".to_owned(), json!("hello"))]);
        save_config(&fixture.store, id, 1, config.clone())
            .await
            .unwrap();
        let saved = fixture.store.installation(id).unwrap().unwrap();
        assert_eq!(saved.config, config);
        assert!(!saved.enabled);
        assert!(matches!(
            save_config(&fixture.store, id, 1, BTreeMap::new()).await,
            Err(ConfigurationError::Conflict)
        ));
        assert!(matches!(
            save_config(
                &fixture.store,
                id,
                saved.updated_at_ms,
                BTreeMap::from([("greeting".to_owned(), json!(false))])
            )
            .await,
            Err(ConfigurationError::InvalidConfig)
        ));
        assert_eq!(fixture.store.installation(id).unwrap().unwrap(), saved);
        save_enabled(&fixture.store, id, saved.updated_at_ms, true)
            .await
            .unwrap();
        let enabled = fixture.store.installation(id).unwrap().unwrap();
        assert!(enabled.enabled);
        assert_eq!(enabled.config, config);
    }

    #[tokio::test]
    async fn enabling_requires_grants_and_package_integrity_but_disabling_does_not_load_code() {
        let fixture = Fixture::new(false);
        let mut installation = fixture.installation.clone();
        installation.granted_permissions.clear();
        fixture.store.upsert_installation(&installation).unwrap();
        let id = &installation.instance_id;
        assert!(matches!(
            save_enabled(&fixture.store, id, 1, true).await,
            Err(ConfigurationError::MissingGrants)
        ));
        std::fs::write(&installation.package_path, b"tampered").unwrap();
        assert!(matches!(
            save_config(&fixture.store, id, 1, BTreeMap::new()).await,
            Err(ConfigurationError::InvalidPackage)
        ));
        save_enabled(&fixture.store, id, 1, false).await.unwrap();
        assert!(!fixture.store.installation(id).unwrap().unwrap().enabled);
    }
}
