use plugin_api::{ActionCompleted, HandlerOutput, PluginEventEnvelope, PluginManifest};

#[test]
fn checked_in_bpp_compatibility_fixtures_parse() {
    let manifest =
        PluginManifest::from_toml(include_str!("../../../test-data/bpp/manifest-valid.toml"))
            .unwrap();
    assert_eq!(manifest.id.as_str(), "dev.bkm.fixture");

    let event: PluginEventEnvelope = serde_json::from_str(include_str!(
        "../../../test-data/bpp/event-message-created.json"
    ))
    .unwrap();
    assert_eq!(event.event_type, "message.created");

    let output: HandlerOutput = serde_json::from_str(include_str!(
        "../../../test-data/bpp/handler-output-reply.json"
    ))
    .unwrap();
    assert_eq!(output.commands[0].kind, "message.reply");

    let completion: ActionCompleted = serde_json::from_str(include_str!(
        "../../../test-data/bpp/action-completed-unknown.json"
    ))
    .unwrap();
    assert_eq!(completion.error_code.as_deref(), Some("result_unknown"));
}
