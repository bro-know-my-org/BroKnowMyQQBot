use qqbot_protocol::{
    C2cMessageStatusEvent, FriendAddEvent, FriendDeleteEvent, FriendScene, GroupMessageStatusEvent,
    GroupRobotEvent, NoticeValidationError,
};
use serde_json::{Value, json};

const SOCIAL_EVENT_KINDS: [&str; 5] = [
    "friend_add",
    "friend_delete",
    "group_robot",
    "group_message_status",
    "c2c_message_status",
];

fn social_fixture(kind: &str) -> Value {
    match kind {
        "friend_add" => json!({
            "openid":"user-openid","timestamp":1_784_570_617_u64,
            "scene":1000,"scene_param":"",
            "author":{"union_openid":"union-openid"}
        }),
        "friend_delete" => json!({
            "openid":"user-openid","timestamp":1_784_570_617_u64,
            "author":{"union_openid":"union-openid"}
        }),
        "group_robot" | "group_message_status" => json!({
            "group_openid":"group-openid","op_member_openid":"operator-openid",
            "timestamp":1_784_570_617_u64
        }),
        "c2c_message_status" => {
            json!({"openid":"user-openid","timestamp":1_784_570_617_u64})
        }
        _ => panic!("unknown social event fixture {kind}"),
    }
}

fn validate_social_fixture(kind: &str, value: Value) -> Result<(), NoticeValidationError> {
    match kind {
        "friend_add" => serde_json::from_value::<FriendAddEvent>(value)
            .unwrap()
            .validate(),
        "friend_delete" => serde_json::from_value::<FriendDeleteEvent>(value)
            .unwrap()
            .validate(),
        "group_robot" => serde_json::from_value::<GroupRobotEvent>(value)
            .unwrap()
            .validate(),
        "group_message_status" => serde_json::from_value::<GroupMessageStatusEvent>(value)
            .unwrap()
            .validate(),
        "c2c_message_status" => serde_json::from_value::<C2cMessageStatusEvent>(value)
            .unwrap()
            .validate(),
        _ => panic!("unknown social event fixture {kind}"),
    }
}

#[test]
fn decodes_current_friend_add_contract_and_preserves_unknown_scene() {
    let event: FriendAddEvent = serde_json::from_value(json!({
        "openid":"user-openid",
        "timestamp":1_784_570_600_u64,
        "scene":9001,
        "scene_param":"callback-data",
        "author":{"union_openid":"union-openid","future":true},
        "short_code":"short-code",
        "future":true
    }))
    .unwrap();

    event.validate().unwrap();
    assert_eq!(event.scene, FriendScene::new(9001));
    assert_eq!(event.scene.value(), 9001);
    assert_eq!(event.author.unwrap().union_openid, "union-openid");
    assert_eq!(event.short_code.as_deref(), Some("short-code"));
    assert_eq!(event.scene_param, "callback-data");
}

#[test]
fn accepts_documented_friend_optional_field_omissions() {
    let add: FriendAddEvent = serde_json::from_value(json!({
        "openid":"user-openid",
        "timestamp":1_784_570_600_u64,
        "scene":1001,
        "scene_param":""
    }))
    .unwrap();
    add.validate().unwrap();
    assert_eq!(add.author, None);
    assert_eq!(add.short_code, None);

    let deleted: FriendDeleteEvent = serde_json::from_value(json!({
        "openid":"user-openid",
        "timestamp":1_784_570_524_u64
    }))
    .unwrap();
    deleted.validate().unwrap();
    assert_eq!(deleted.author, None);
}

#[test]
fn validates_c2c_message_status_and_rejects_non_integer_timestamps() {
    let status: C2cMessageStatusEvent = serde_json::from_value(json!({
        "openid":"user-openid",
        "timestamp":1_784_570_617_u64
    }))
    .unwrap();
    status.validate().unwrap();

    for timestamp in [json!("2026-08-22T10:00:00Z"), json!(-1)] {
        assert!(
            serde_json::from_value::<C2cMessageStatusEvent>(json!({
                "openid":"user-openid",
                "timestamp":timestamp
            }))
            .is_err()
        );
    }
}

#[test]
fn validates_every_social_notice_identifier_field() {
    let cases = [
        ("friend_add", "/openid", "openid"),
        ("friend_add", "/author/union_openid", "author.union_openid"),
        ("friend_delete", "/openid", "openid"),
        (
            "friend_delete",
            "/author/union_openid",
            "author.union_openid",
        ),
        ("group_robot", "/group_openid", "group_openid"),
        ("group_robot", "/op_member_openid", "op_member_openid"),
        ("group_message_status", "/group_openid", "group_openid"),
        (
            "group_message_status",
            "/op_member_openid",
            "op_member_openid",
        ),
        ("c2c_message_status", "/openid", "openid"),
    ];

    for (kind, pointer, field) in cases {
        for (invalid, expected) in [
            ("", NoticeValidationError::EmptyField { field }),
            (" ", NoticeValidationError::InvalidIdentifier { field }),
            (
                "invalid id",
                NoticeValidationError::InvalidIdentifier { field },
            ),
            (
                "invalid\tid",
                NoticeValidationError::InvalidIdentifier { field },
            ),
            (
                "invalid\0id",
                NoticeValidationError::InvalidIdentifier { field },
            ),
        ] {
            let mut fixture = social_fixture(kind);
            *fixture.pointer_mut(pointer).unwrap() = Value::String(invalid.to_owned());
            assert_eq!(validate_social_fixture(kind, fixture), Err(expected));
        }
    }
}

#[test]
fn social_notice_validators_enforce_chrono_timestamp_boundary() {
    let max_timestamp =
        u64::try_from(chrono::DateTime::<chrono::Utc>::MAX_UTC.timestamp()).unwrap();
    let invalid_timestamp = max_timestamp.checked_add(1).unwrap();
    let expected = NoticeValidationError::InvalidUnixTimestamp { field: "timestamp" };

    for kind in SOCIAL_EVENT_KINDS {
        let mut valid = social_fixture(kind);
        valid["timestamp"] = json!(max_timestamp);
        validate_social_fixture(kind, valid).unwrap();

        let mut invalid = social_fixture(kind);
        invalid["timestamp"] = json!(invalid_timestamp);
        assert_eq!(validate_social_fixture(kind, invalid), Err(expected));
    }
}
