use plugin_api::{
    HandlerOutput, ManifestError, PluginCommand, PluginEventEnvelope, PluginManifest,
    QQ_BUTTON_ACK, QQ_BUTTON_CLICKED, QQ_MARKDOWN_REPLY, QQ_MARKDOWN_SEND, QqButtonAck,
    QqButtonClicked, QqChat, QqMarkdown, QqMarkdownSend,
};
use serde_json::{Value, json};

const LEGACY_MANIFEST: &str = include_str!("../../../test-data/bpp/manifest-valid.toml");
const CALLBACK_EVENT: &str = include_str!("../../../test-data/bpp/event-qq-button-clicked.json");
const QQ_OUTPUT: &str = include_str!("../../../test-data/bpp/handler-output-qq.json");

#[test]
fn qq_command_and_event_fixtures_round_trip_through_typed_contracts() {
    let source: Value = serde_json::from_str(QQ_OUTPUT).unwrap();
    let output: HandlerOutput = serde_json::from_value(source.clone()).unwrap();
    let [reply, send, ack] = output.commands.as_slice() else {
        panic!("expected a reply, a send, and an ACK");
    };
    let reply_body: QqMarkdown = serde_json::from_value(reply.payload.clone()).unwrap();
    let send_body: QqMarkdownSend = serde_json::from_value(send.payload.clone()).unwrap();
    let ack_result: QqButtonAck = serde_json::from_value(ack.payload.clone()).unwrap();
    assert!(reply_body.force_verify_image_resource);
    reply_body.validate().unwrap();
    send_body.markdown.validate().unwrap();
    send_body
        .markdown
        .validate_target(&send_body.target)
        .unwrap();
    let commands = [
        PluginCommand::qq_markdown_reply("reply", &reply_body),
        PluginCommand::qq_markdown_send("send", &send_body),
        PluginCommand::qq_button_ack("ack", ack_result),
    ];
    assert_eq!(serde_json::to_value(commands).unwrap(), source["commands"]);
    assert_eq!(serde_json::to_value(output).unwrap(), source);

    let source: Value = serde_json::from_str(CALLBACK_EVENT).unwrap();
    let event: PluginEventEnvelope = serde_json::from_value(source.clone()).unwrap();
    let clicked = event.qq_button_clicked().unwrap().unwrap();
    assert_eq!(
        clicked,
        QqButtonClicked {
            actor_id: "fixture-user-1".to_owned(),
            target: QqChat::Private {
                user_id: "fixture-user-1".to_owned(),
            },
            button_id: "next".to_owned(),
            data: "page=2".to_owned(),
        }
    );
    assert_eq!(event.extensions, []);
    assert_eq!(serde_json::to_value(clicked).unwrap(), source["payload"]);
    assert_eq!(serde_json::to_value(event).unwrap(), source);

    let legacy_event: PluginEventEnvelope = serde_json::from_str(include_str!(
        "../../../test-data/bpp/event-message-created.json"
    ))
    .unwrap();
    assert_eq!(legacy_event.qq_button_clicked().unwrap(), None);
}

#[test]
fn qq_markdown_and_keyboard_require_exclusive_typed_shapes() {
    let markdown = fixture_markdown();
    for (path, replacement) in [
        (
            "/body",
            json!({"type": "content", "content": "# Page", "id": "template"}),
        ),
        (
            "/body",
            json!({"type": "template", "id": "template", "params": [], "content": "# Page"}),
        ),
        (
            "/body",
            json!({"type": "template", "id": "template", "params": [{"key": "title", "values": ["Page"], "extra": true}]}),
        ),
        (
            "/keyboard",
            json!({"type": "template", "id": "keys", "rows": []}),
        ),
        (
            "/keyboard",
            json!({"type": "inline", "id": "keys", "rows": []}),
        ),
        ("/keyboard/rows/0/0/style", json!(1)),
        (
            "/keyboard/rows/0/0/action",
            json!({"type": "link", "url": "https://example.test", "enter": false}),
        ),
        (
            "/keyboard/rows/0/0/action",
            json!({"type": "command", "text": "/page", "enter": false}),
        ),
        (
            "/keyboard/rows/0/0/action",
            json!({"type": "callback", "data": "next", "reply": false}),
        ),
        (
            "/keyboard/rows/0/0/access",
            json!({"type": "users", "ids": ["fixture-user-1"], "admin": true}),
        ),
    ] {
        let mut invalid = markdown.clone();
        replace(&mut invalid, path, replacement);
        assert!(
            serde_json::from_value::<QqMarkdown>(invalid.clone()).is_err(),
            "accepted malformed shape at {path}: {invalid}"
        );
    }

    let mut reply = markdown;
    reply["target"] = json!({"scope": "private", "user_id": "fixture-user-1"});
    assert!(serde_json::from_value::<QqMarkdown>(reply).is_err());
    let minimal: QqMarkdown =
        serde_json::from_value(json!({"body": {"type": "content", "content": "# Page"}})).unwrap();
    assert!(minimal.keyboard.is_none());
    assert!(!minimal.force_verify_image_resource);
    minimal.validate().unwrap();
}

#[test]
fn qq_access_variants_without_user_lists_reject_extra_fields() {
    let mut accepted = Vec::new();
    for access in ["everyone", "administrators"] {
        let mut markdown = fixture_markdown();
        markdown["keyboard"]["rows"][0][0]["access"] =
            json!({"type": access, "ids": ["fixture-user-1"]});
        if serde_json::from_value::<QqMarkdown>(markdown).is_ok() {
            accepted.push(access);
        }
    }
    assert!(accepted.is_empty(), "accepted user lists for {accepted:?}");
}

#[test]
fn qq_markdown_validates_text_and_template_bounds() {
    let mut markdown = fixture_markdown();
    markdown["body"]["content"] = json!("界".repeat(5461) + "a");
    assert_valid(&markdown);
    for content in ["界".repeat(5462), "  \n".to_owned(), "a\0b".to_owned()] {
        markdown["body"]["content"] = json!(content);
        assert_invalid(&markdown, "Markdown content");
    }

    markdown["body"] = json!({
        "type": "template",
        "id": "m".repeat(128),
        "params": (0..20).map(|index| json!({
            "key": format!("{index:0128}"),
            "values": vec!["v".repeat(2048); 20],
        })).collect::<Vec<_>>(),
    });
    assert_valid(&markdown);
    let mut too_many_params = markdown["body"]["params"].as_array().unwrap().clone();
    too_many_params.push(json!({"key": "extra", "values": ["value"]}));
    for (path, replacement) in [
        ("/body/id", json!("m".repeat(129))),
        ("/body/params", json!(too_many_params)),
        (
            "/body/params/1/key",
            markdown["body"]["params"][0]["key"].clone(),
        ),
        ("/body/params/0/key", json!("bad key")),
        ("/body/params/0/values", json!([])),
        ("/body/params/0/values", json!(vec!["v"; 21])),
        ("/body/params/0/values/0", json!("v".repeat(2049))),
        ("/body/params/0/values/0", json!("a\0b")),
    ] {
        let mut invalid = markdown.clone();
        replace(&mut invalid, path, replacement);
        assert_invalid(&invalid, path);
    }
}

#[test]
fn qq_keyboard_validates_size_unique_ids_and_button_bounds() {
    let mut markdown = fixture_markdown();
    let base = &markdown["keyboard"]["rows"][0][0];
    let mut rows = Vec::new();
    for row in 0..5 {
        let mut buttons = Vec::new();
        for column in 0..5 {
            let mut button = base.clone();
            button["id"] = json!(format!("{row}{column}"));
            buttons.push(button);
        }
        rows.push(buttons);
    }
    markdown["keyboard"]["rows"] = json!(rows);
    assert_valid(&markdown);
    let mut duplicate = markdown.clone();
    duplicate["keyboard"]["rows"][4][4]["id"] = json!("00");
    assert_invalid(&duplicate, "duplicate across rows");
    rows.push(vec![rows[0][0].clone()]);
    // Keep the sixth row's button unique so the row count is the only invalid field.
    rows[5][0]["id"] = json!("50");
    markdown["keyboard"]["rows"] = json!(rows);
    assert_invalid(&markdown, "six rows");
    rows.pop();
    let mut extra = rows[0][0].clone();
    extra["id"] = json!("05");
    rows[0].push(extra);
    markdown["keyboard"]["rows"] = json!(rows);
    assert_invalid(&markdown, "six buttons");
    for rows in [json!([]), json!([[]])] {
        markdown["keyboard"]["rows"] = rows;
        assert_invalid(&markdown, "empty keyboard or row");
    }

    let mut markdown = fixture_markdown();
    let button = &mut markdown["keyboard"]["rows"][0][0];
    button["id"] = json!("b".repeat(64));
    button["label"] = json!("l".repeat(64));
    button["visited_label"] = json!("v".repeat(64));
    button["unsupported_tips"] = json!("t".repeat(256));
    button["access"]["ids"] = json!(
        (0..20)
            .map(|index| format!("{index:0256}"))
            .collect::<Vec<_>>()
    );
    button["action"]["data"] = json!("d".repeat(1024));
    assert_valid(&markdown);
    for (field, replacement) in [
        ("/id", json!("b".repeat(65))),
        ("/label", json!("l".repeat(65))),
        ("/visited_label", json!("line\nbreak")),
        ("/unsupported_tips", json!("t".repeat(257))),
        ("/access/ids", json!([])),
        ("/access/ids", json!(["same", "same"])),
        (
            "/access/ids",
            json!((0..21).map(|index| index.to_string()).collect::<Vec<_>>()),
        ),
        ("/access/ids/0", json!("u".repeat(257))),
        ("/action/data", json!("d".repeat(1025))),
    ] {
        let mut invalid = markdown.clone();
        replace(&mut invalid["keyboard"]["rows"][0][0], field, replacement);
        assert_invalid(&invalid, field);
    }

    markdown["keyboard"] = json!({"type": "template", "id": "k".repeat(128)});
    assert_valid(&markdown);
    markdown["keyboard"]["id"] = json!("k".repeat(129));
    assert_invalid(&markdown, "Keyboard template ID");
}

#[test]
fn qq_buttons_validate_action_and_conversation_restrictions() {
    let group = QqChat::Group {
        group_id: "group-1".to_owned(),
    };
    let private = QqChat::Private {
        user_id: "user-1".to_owned(),
    };
    let mut markdown = fixture_markdown();
    markdown["keyboard"]["rows"][0][0]["action"] =
        json!({"type": "command", "text": "/page", "enter": true, "reply": false});
    let command: QqMarkdown = serde_json::from_value(markdown.clone()).unwrap();
    command.validate().unwrap();
    assert!(command.validate_target(&group).is_err());
    command.validate_target(&private).unwrap();

    let button = &mut markdown["keyboard"]["rows"][0][0];
    button["action"]["enter"] = json!(false);
    button["access"] = json!({"type": "administrators"});
    let admin: QqMarkdown = serde_json::from_value(markdown.clone()).unwrap();
    admin.validate().unwrap();
    admin.validate_target(&group).unwrap();
    assert!(admin.validate_target(&private).is_err());
    markdown["keyboard"]["rows"][0][0]["action"]["text"] = json!("c".repeat(1025));
    assert_invalid(&markdown, "command exceeds 1024 bytes");

    for (url, valid) in [
        ("https://example.test/path", true),
        ("http://example.test", true),
        ("/relative", false),
        ("ftp://example.test", false),
        ("https://user:password@example.test", false),
        ("https://example.test/a b", false),
    ] {
        markdown["keyboard"]["rows"][0][0]["action"] = json!({"type": "link", "url": url});
        let link: QqMarkdown = serde_json::from_value(markdown.clone()).unwrap();
        assert_eq!(link.validate().is_ok(), valid, "{url}");
    }

    for target in [
        json!({"scope": "channel", "channel_id": "channel-1"}),
        json!({"scope": "guild_direct", "guild_id": "guild-1"}),
        json!({"scope": "group", "group_id": "group-1", "user_id": "user-1"}),
    ] {
        assert!(serde_json::from_value::<QqChat>(target).is_err());
    }
    for user_id in ["", "bad user", "bad\nuser", &"u".repeat(257)] {
        assert!(
            QqChat::Private {
                user_id: user_id.to_owned()
            }
            .validate()
            .is_err()
        );
    }
    QqChat::Group {
        group_id: "g".repeat(256),
    }
    .validate()
    .unwrap();
}

#[test]
fn qq_ack_accepts_only_named_results_and_callback_projection_excludes_interaction_id() {
    for (code, name) in [
        "success",
        "failure",
        "too_frequent",
        "duplicate",
        "no_permission",
        "only_administrator",
    ]
    .into_iter()
    .enumerate()
    {
        let ack: QqButtonAck = serde_json::from_value(json!(name)).unwrap();
        assert_eq!(usize::from(ack.code()), code);
        assert_eq!(
            PluginCommand::qq_button_ack("confirm", ack).payload,
            json!(name)
        );
    }
    for payload in [
        json!(0),
        json!("unknown"),
        json!({"code": "success", "interaction_id": "fixture-interaction"}),
        json!({"success": {"interaction_id": "fixture-interaction"}}),
    ] {
        assert!(serde_json::from_value::<QqButtonAck>(payload).is_err());
    }
    let mut event: PluginEventEnvelope = serde_json::from_str(CALLBACK_EVENT).unwrap();
    event.payload["interaction_id"] = json!("fixture-interaction");
    assert!(event.qq_button_clicked().is_err());
}

#[test]
fn each_qq_feature_independently_requires_bpp_1_7() {
    let legacy = PluginManifest::from_toml(LEGACY_MANIFEST).unwrap();
    assert!(!legacy.uses_qq_rich_messages());
    for action in [
        Some(QQ_MARKDOWN_REPLY),
        Some(QQ_MARKDOWN_SEND),
        Some(QQ_BUTTON_ACK),
        None,
    ] {
        let mut manifest = legacy.clone();
        if let Some(action) = action {
            manifest.permissions.actions.insert(action.to_owned());
            assert!(manifest.requested_capabilities().contains(action));
        } else {
            manifest.subscriptions[0].event = QQ_BUTTON_CLICKED.to_owned();
        }
        assert!(manifest.uses_qq_rich_messages());
        for protocol in [">=1.0,<2.0", ">=1.6,<2.0", ">1.6.123,<2.0"] {
            manifest.protocol = protocol.to_owned();
            assert!(
                matches!(
                    manifest.validate(),
                    Err(ManifestError::FeatureRequiresProtocol17)
                ),
                "{action:?} with {protocol}"
            );
        }
        manifest.protocol = ">=1.7,<2.0".to_owned();
        manifest.validate().unwrap();
    }

    let mut arbitrary_qq_action = legacy;
    arbitrary_qq_action.protocol = ">=1.7,<2.0".to_owned();
    arbitrary_qq_action
        .permissions
        .actions
        .insert("qq.interaction.respond".to_owned());
    assert!(matches!(
        arbitrary_qq_action.validate(),
        Err(ManifestError::UnsupportedAction(action)) if action == "qq.interaction.respond"
    ));
}

fn fixture_markdown() -> Value {
    let output: Value = serde_json::from_str(QQ_OUTPUT).unwrap();
    output["commands"][0]["payload"].clone()
}

fn replace(value: &mut Value, path: &str, replacement: Value) {
    *value
        .pointer_mut(path)
        .expect("fixture must contain the field") = replacement;
}

fn assert_valid(value: &Value) {
    serde_json::from_value::<QqMarkdown>(value.clone())
        .unwrap()
        .validate()
        .unwrap();
}

fn assert_invalid(value: &Value, case: &str) {
    let markdown: QqMarkdown = serde_json::from_value(value.clone()).unwrap();
    assert!(markdown.validate().is_err(), "accepted invalid {case}");
}
