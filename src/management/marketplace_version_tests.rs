use super::*;

async fn installed(
    saved: &str,
    loaded: Option<&str>,
    candidate: &str,
) -> (Fixture, PluginManager, MarketplaceFixture) {
    let (fixture, _, data) = fixture();
    let base = data.lock().unwrap().1.clone();
    let bytes = versioned_package(&base, saved);
    let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
    let mut record = fixture.installation.clone();
    record.instance_id = "dev.bkm.wasm-ping/default".into();
    record.version = saved.to_owned();
    record.config.insert("greeting".into(), json!("keep"));
    let path = fixture.directory.join("saved.bkm-plugin");
    std::fs::write(&path, &bytes).unwrap();
    record.package_path = path.to_str().unwrap().to_owned();
    record.package_sha256 = package.package_sha256().to_owned();
    fixture.store.upsert_installation(&record).unwrap();
    let mut host = StaticPluginHost::new(fixture.store.clone());
    let mut packages = BTreeMap::new();
    if let Some(version) = loaded {
        let bytes = versioned_package(&base, version);
        let package = ValidatedPluginPackage::from_bytes(&bytes).unwrap();
        packages.insert(
            record.instance_id.clone(),
            package.package_sha256().to_owned(),
        );
        host.register(
            Arc::new(
                plugin_host::WasmPlugin::from_package(package)
                    .await
                    .unwrap(),
            ),
            &record.instance_id,
            record.config.clone(),
            record.granted_permissions.iter().cloned().collect(),
        )
        .await
        .unwrap();
    }
    let mut manager = PluginManager::new(host, fixture.store.clone(), packages, BTreeSet::new())
        .with_marketplace(
            fixture.directory.join("plugins.db"),
            None,
            DEFAULT_MARKETPLACE_URL.into(),
        )
        .unwrap();
    manager.use_marketplace_fixture(data.clone());
    set_market_version(&data, candidate);
    (fixture, manager, data)
}

#[tokio::test]
async fn marketplace_rejects_same_or_older_versions_before_download_without_side_effects() {
    for (candidate, saved, loaded) in [
        ("0.2.6", "0.4.0", Some("0.4.0")),
        ("0.4.0", "0.4.0", Some("0.4.0")),
        ("0.3.0", "0.2.6", Some("0.4.0")),
        ("1.0.0+new", "1.0.0+old", None),
    ] {
        let (fixture, manager, data) = installed(saved, loaded, candidate).await;
        let id = "dev.bkm.wasm-ping/default";
        let before = fixture.store.installation(id).unwrap().unwrap();
        let app = app(manager.clone());
        let body = intent(app.clone()).await;
        // Reaching package download would fail its hash check with 422 instead of the version 409.
        data.lock().unwrap().1 = b"must not be downloaded".to_vec();
        let (status, error) = send(app, "/api/marketplace/update", Some(body)).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "{candidate} {saved} {loaded:?}"
        );
        assert!(error["error"].as_str().unwrap().contains("已拒绝更新"));
        assert_eq!(fixture.store.installation(id).unwrap().unwrap(), before);
        assert!(!fixture.directory.join("plugins").exists());
        let live = serde_json::to_value(manager.view(id).await.unwrap()).unwrap();
        assert_eq!(live["loaded_version"], json!(loaded));
        if loaded.is_some() {
            assert_eq!(live["runtime_state"], "ready");
        }
        manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn marketplace_accepts_newer_semver_and_correction_to_the_loaded_version() {
    for (candidate, saved, loaded) in [
        ("0.4.0", "0.2.6", Some("0.4.0")),
        ("0.10.0", "0.9.0", Some("0.9.0")),
        ("1.0.0", "1.0.0-rc.1", None),
    ] {
        let (fixture, manager, _) = installed(saved, loaded, candidate).await;
        let app = app(manager.clone());
        let body = intent(app.clone()).await;
        let (status, result) = send(app, "/api/marketplace/update", Some(body)).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["version"], candidate);
        assert_eq!(result["loaded_version"], json!(loaded));
        assert_eq!(result["restart_required"], true);
        let record = fixture
            .store
            .installation("dev.bkm.wasm-ping/default")
            .unwrap()
            .unwrap();
        assert!(record.enabled);
        assert_eq!(record.config["greeting"], "keep");
        assert_eq!(
            record.granted_permissions,
            fixture.installation.granted_permissions
        );
        manager.shutdown().await.unwrap();
    }
}
