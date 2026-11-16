use super::*;
use base64::Engine as _;
use plugin_api::PluginCommand;
use std::collections::BTreeSet;

struct CardPlugin {
    manifest: PluginManifest,
    payload: Value,
    copies: usize,
}

fn image_payload() -> Value {
    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(1, 1)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    json!({"mime_type":"image/png","data_base64":base64::engine::general_purpose::STANDARD.encode(png.into_inner())})
}

impl CardPlugin {
    fn new(payload: Value) -> Self {
        let mut manifest = PingPlugin::default().manifest().clone();
        ">=1.5,<2.0".clone_into(&mut manifest.protocol);
        manifest.permissions.actions =
            BTreeSet::from(["media.create".to_owned(), "media.reply".to_owned()]);
        manifest.permissions.storage = plugin_api::StoragePermission::Private;
        manifest.permissions.storage_quota_bytes = 1024;
        manifest.subscriptions.push(plugin_api::Subscription {
            id: "results".to_owned(),
            event: "action.completed".to_owned(),
            priority: 0,
            scopes: BTreeSet::new(),
        });
        Self {
            manifest,
            payload,
            copies: 1,
        }
    }
}

#[async_trait]
impl StaticPlugin for CardPlugin {
    fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }
    async fn on_event(
        &self,
        event: &PluginEventEnvelope,
        _: &dyn HostQueries,
    ) -> Result<HandlerOutput, PluginError> {
        let (kind, payload) = if event.event_type == "message.created" {
            ("media.create", self.payload.clone())
        } else {
            let completion: ActionCompleted =
                serde_json::from_value(event.payload.clone()).unwrap();
            assert_eq!(completion.status, ActionStatus::Succeeded);
            if completion.kind != "media.create" {
                return Ok(HandlerOutput::default());
            }
            let reference: plugin_api::AssetReference =
                serde_json::from_value(completion.result.unwrap()).unwrap();
            assert_eq!(reference.mime_type, "image/png");
            assert_eq!(reference.sha256.len(), 64);
            assert!(reference.size_bytes > 8);
            (
                "media.reply",
                json!({"asset_id":reference.asset_id, "caption":"card"}),
            )
        };
        Ok(HandlerOutput {
            commands: (0..self.copies)
                .map(|index| PluginCommand {
                    command_id: format!("{kind}/{index}"),
                    kind: kind.to_owned(),
                    payload: payload.clone(),
                    idempotency_key: None,
                    deadline_ms: None,
                })
                .collect(),
            state_ops: vec![StateOp::Put {
                key: "card".to_owned(),
                value: vec![1],
                expected_revision: None,
            }],
            ..HandlerOutput::default()
        })
    }
}

async fn run_card(
    payload: Value,
    grant: bool,
) -> (Option<Action>, BTreeMap<String, plugin_api::StateValue>) {
    run_card_copies(payload, grant, 1).await
}

async fn run_card_copies(
    payload: Value,
    grant: bool,
    copies: usize,
) -> (Option<Action>, BTreeMap<String, plugin_api::StateValue>) {
    let (shutdown, signal) = shutdown_channel();
    let (sender, mut actions) = mpsc::channel(1);
    let adapter = Arc::new(MockAdapter {
        id: AdapterId::new("mock"),
        event: Mutex::new(Some(message_event("/ping"))),
        actions: sender,
        shutdown,
        outcome: ActionOutcome::Succeeded,
        shutdown_after_send: true,
    });
    let store = PluginStore::in_memory().unwrap();
    let mut host = StaticPluginHost::new(store.clone()).with_adapter(adapter.clone());
    let mut plugin = CardPlugin::new(payload);
    plugin.copies = copies;
    let mut grants = plugin.manifest.requested_capabilities();
    if !grant {
        grants.remove("media.create");
    }
    host.register(Arc::new(plugin), "card/test", BTreeMap::new(), grants)
        .await
        .unwrap();
    let runtime = RuntimeBuilder::new()
        .adapter(adapter)
        .handler(Arc::new(host))
        .build()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), runtime.run(signal))
        .await
        .unwrap()
        .unwrap();
    (
        actions.try_recv().ok(),
        store.snapshot("card/test").unwrap(),
    )
}

#[tokio::test]
async fn creation_receipt_chains_to_original_image_reply() {
    let (action, state) = run_card(image_payload(), true).await;
    let Some(Action::ReplyMedia(reply)) = action else {
        panic!("missing image reply")
    };
    assert_eq!(reply.source_message_id, "source-message");
    assert_eq!(
        reply.target,
        MessageTarget::Group {
            group_id: "group".to_owned()
        }
    );
    assert_eq!(reply.caption.as_deref(), Some("card"));
    assert_eq!(reply.attachment.mime_type(), "image/png");
    assert!(state.contains_key("card"));
}

#[tokio::test]
async fn permission_and_invalid_image_reject_state_atomically() {
    for (payload, grant) in [
        (image_payload(), false),
        (json!({"mime_type":"image/png","data_base64":"bad"}), true),
    ] {
        let (action, state) = run_card(payload, grant).await;
        assert!(action.is_none());
        assert!(state.is_empty());
    }
}

#[tokio::test]
async fn aggregate_output_rejects_otherwise_valid_image_commands_atomically() {
    let mut seed = 1_u32;
    let bytes: Vec<_> = (0..256 * 256 * 3)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed.to_le_bytes()[0]
        })
        .collect();
    let image = image::RgbImage::from_raw(256, 256, bytes).unwrap();
    let mut encoded = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let payload = json!({"mime_type":"image/png","data_base64":base64::engine::general_purpose::STANDARD.encode(encoded.into_inner())});
    assert!(payload.to_string().len() < 1024 * 1024);
    assert!(run_card(payload.clone(), true).await.0.is_some());
    let (action, state) = run_card_copies(payload, true, 5).await;
    assert!(action.is_none());
    assert!(state.is_empty());
}
