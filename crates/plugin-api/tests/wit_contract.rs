use std::path::PathBuf;

use wit_parser::{Resolve, Type, TypeDefKind};

#[test]
fn formal_wit_contract_parses_with_expected_identity() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("wit/bkm-plugin.wit")
        .canonicalize()
        .expect("formal BPP WIT must exist");
    let mut resolve = Resolve::default();
    let (package_id, _) = resolve
        .push_path(&path)
        .expect("formal BPP WIT must parse and resolve");
    let package = &resolve.packages[package_id];
    assert_eq!(package.name.namespace, "bkm");
    assert_eq!(package.name.name, "plugin");
    assert_eq!(
        package.name.version.as_ref().map(ToString::to_string),
        Some(plugin_api::BPP_VERSION.to_owned())
    );
    assert!(package.worlds.contains_key("plugin"));
    assert!(package.interfaces.contains_key("types"));
    assert!(package.interfaces.contains_key("queries"));
    assert!(package.interfaces.contains_key("lifecycle"));
    assert!(package.interfaces.contains_key("handler"));
    let types_id = package.interfaces["types"];
    let types = &resolve.interfaces[types_id];
    let queries = &resolve.interfaces[package.interfaces["queries"]];
    assert!(queries.functions.contains_key("caller-authority"));
    assert!(queries.functions.contains_key("command-context"));
    assert!(queries.functions.contains_key("command-prefix"));
    let TypeDefKind::Record(command) = &resolve.types[types.types["command-context"]].kind else {
        panic!("BPP command-context must be a record");
    };
    assert_eq!(
        command
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["prefix", "name", "arguments"]
    );
    let TypeDefKind::Enum(roles) = &resolve.types[types.types["caller-role"]].kind else {
        panic!("BPP caller-role must be an enum");
    };
    assert_eq!(
        roles
            .cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>(),
        ["user", "admin", "owner"]
    );
    let message_target_id = types.types["message-target"];
    let TypeDefKind::Variant(message_target) = &resolve.types[message_target_id].kind else {
        panic!("BPP message-target must be a variant");
    };
    assert_eq!(
        message_target
            .cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>(),
        ["group", "private", "channel", "guild-direct"]
    );
    let command_payload_id = types.types["command-payload"];
    let TypeDefKind::Variant(command_payload) = &resolve.types[command_payload_id].kind else {
        panic!("BPP command-payload must be a variant");
    };
    let expected = [
        ("message-reply", "message-reply-command"),
        ("message-send", "message-send-command"),
        ("media-reply", "media-reply-command"),
        ("media-send", "media-send-command"),
        ("http-request", "http-request-command"),
        ("browser-run", "browser-run-command"),
        ("schedule-create", "schedule-create-command"),
        ("schedule-cancel", "schedule-cancel-command"),
        ("media-create", "media-create-command"),
    ];
    assert_eq!(command_payload.cases.len(), expected.len());
    for (case, (expected_case, expected_payload)) in command_payload.cases.iter().zip(expected) {
        assert_eq!(case.name, expected_case);
        let Some(Type::Id(payload_id)) = case.ty else {
            panic!("BPP command `{expected_case}` must carry a typed payload");
        };
        assert_eq!(
            resolve.types[payload_id].name.as_deref(),
            Some(expected_payload)
        );
    }
}
