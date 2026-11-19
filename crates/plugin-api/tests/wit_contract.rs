use std::path::PathBuf;

use wit_parser::{Resolve, Type, TypeDefKind, TypeId, WorldItem};

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
    assert_management_contract(&resolve, package);
    assert_qq_contract(&resolve, package);
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
    assert_variant_cases(
        &resolve,
        types.types["command-payload"],
        &[
            ("message-reply", "message-reply-command"),
            ("message-send", "message-send-command"),
            ("media-reply", "media-reply-command"),
            ("media-send", "media-send-command"),
            ("http-request", "http-request-command"),
            ("browser-run", "browser-run-command"),
            ("schedule-create", "schedule-create-command"),
            ("schedule-cancel", "schedule-cancel-command"),
            ("media-create", "media-create-command"),
        ],
    );
    assert_variant_cases(
        &resolve,
        types.types["event-payload"],
        &[
            ("message-created", "message-created"),
            ("notice", "encoded-value"),
            ("request", "encoded-value"),
            ("lifecycle", "encoded-value"),
            ("action-completed", "action-completed"),
            ("schedule-triggered", "schedule-triggered"),
            ("opaque", "encoded-value"),
        ],
    );
}

fn assert_variant_cases(resolve: &Resolve, id: TypeId, expected: &[(&str, &str)]) {
    let ty = &resolve.types[id];
    let TypeDefKind::Variant(variant) = &ty.kind else {
        panic!("{:?} must be a variant", ty.name);
    };
    assert_eq!(variant.cases.len(), expected.len());
    for (case, &(expected_case, expected_payload)) in variant.cases.iter().zip(expected) {
        assert_eq!(case.name, expected_case);
        let Some(Type::Id(payload_id)) = case.ty else {
            panic!("BPP case `{expected_case}` must carry a named payload type");
        };
        assert_eq!(
            resolve.types[payload_id].name.as_deref(),
            Some(expected_payload)
        );
    }
}

fn assert_qq_contract(resolve: &Resolve, package: &wit_parser::Package) {
    for (world_name, handler) in [("plugin", "handler"), ("qq-plugin", "qq-handler")] {
        let world = &resolve.worlds[package.worlds[world_name]];
        let interface_name = |item: &WorldItem| match item {
            WorldItem::Interface { id, .. } => resolve.interfaces[*id].name.as_deref().unwrap(),
            _ => panic!("{world_name} must only import or export interfaces"),
        };
        let mut exports: Vec<_> = world.exports.values().map(interface_name).collect();
        let mut expected_exports = ["lifecycle", handler];
        exports.sort_unstable();
        expected_exports.sort_unstable();
        assert_eq!(exports, expected_exports);
        assert!(
            world
                .imports
                .values()
                .any(|item| interface_name(item) == "queries")
        );
    }

    let qq = &resolve.interfaces[package.interfaces["qq-handler"]];
    assert_variant_cases(
        resolve,
        qq.types["event"],
        &[("core", "event-envelope"), ("button", "button-envelope")],
    );
    assert_variant_cases(
        resolve,
        qq.types["qq-command-payload"],
        &[
            ("core", "command-payload"),
            ("markdown-reply", "markdown"),
            ("markdown-send", "markdown-send"),
            ("button-ack", "ack-result"),
        ],
    );
    let TypeDefKind::Enum(ack) = &resolve.types[qq.types["ack-result"]].kind else {
        panic!("button ACK must be a fieldless enum");
    };
    assert_eq!(
        ack.cases
            .iter()
            .map(|case| case.name.as_str())
            .collect::<Vec<_>>(),
        [
            "success",
            "failure",
            "too-frequent",
            "duplicate",
            "no-permission",
            "only-administrator"
        ]
    );
    let TypeDefKind::Record(callback) = &resolve.types[qq.types["button-clicked"]].kind else {
        panic!("QQ callback must carry a typed record");
    };
    assert_eq!(
        callback
            .fields
            .iter()
            .map(|field| (field.name.as_str(), field.ty))
            .collect::<Vec<_>>(),
        [
            ("actor-id", Type::String),
            ("target", Type::Id(qq.types["qq-chat"])),
            ("button-id", Type::String),
            ("data", Type::String),
        ]
    );
    let TypeDefKind::Record(envelope) = &resolve.types[qq.types["button-envelope"]].kind else {
        panic!("QQ callback envelope must be a record");
    };
    assert_eq!(
        envelope
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        [
            "protocol-version",
            "event-id",
            "delivery-id",
            "invocation-id",
            "occurred-at-ms",
            "received-at-ms",
            "adapter-id",
            "trace-id",
            "button",
        ]
    );
    assert_eq!(envelope.fields[8].ty, Type::Id(qq.types["button-clicked"]));
    assert_qq_handler_output(resolve, qq);
}

fn assert_qq_handler_output(resolve: &Resolve, qq: &wit_parser::Interface) {
    let on_event = &qq.functions["on-event"];
    assert_eq!(on_event.params.len(), 1);
    assert_eq!(on_event.params[0].ty, Type::Id(qq.types["event"]));
    let Some(Type::Id(result)) = on_event.result else {
        panic!("QQ handler must return a typed result");
    };
    let TypeDefKind::Result(result) = &resolve.types[result].kind else {
        panic!("QQ handler must return a result");
    };
    assert_eq!(result.ok, Some(Type::Id(qq.types["output"])));
    let TypeDefKind::Record(output) = &resolve.types[qq.types["output"]].kind else {
        panic!("QQ handler output must be a record");
    };
    let Some(Type::Id(commands)) = output
        .fields
        .iter()
        .find(|field| field.name == "commands")
        .map(|field| field.ty)
    else {
        panic!("QQ handler output must carry typed commands");
    };
    assert!(matches!(
        resolve.types[commands].kind,
        TypeDefKind::List(Type::Id(command)) if command == qq.types["command"]
    ));
    let TypeDefKind::Record(command) = &resolve.types[qq.types["command"]].kind else {
        panic!("QQ command must be a record");
    };
    assert_eq!(
        command
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["command-id", "idempotency-key", "deadline-ms", "payload"]
    );
    assert_eq!(
        command.fields[3].ty,
        Type::Id(qq.types["qq-command-payload"])
    );
}

fn assert_management_contract(resolve: &Resolve, package: &wit_parser::Package) {
    assert!(package.worlds.contains_key("managed-plugin"));
    let management = &resolve.interfaces[package.interfaces["management"]];
    assert_eq!(management.functions.len(), 1);
    assert!(management.functions.contains_key("invoke"));
    let TypeDefKind::Record(response) = &resolve.types[management.types["response"]].kind else {
        panic!("management response must be a record");
    };
    assert_eq!(
        response
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect::<Vec<_>>(),
        ["data", "state-ops"]
    );
}
