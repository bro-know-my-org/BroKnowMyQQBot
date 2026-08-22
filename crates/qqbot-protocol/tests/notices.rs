use qqbot_protocol::{
    MessageAuditEvent, MessageAuditOutcome, MessageDeleteEvent, NoticeValidationError,
    SubscribeMessageStatusEvent,
};
use serde_json::json;

fn deleted_message() -> serde_json::Value {
    json!({
        "message": {
            "id": "message-id",
            "guild_id": "guild-id",
            "channel_id": "channel-id",
            "timestamp": "2026-08-22T10:00:00+08:00",
            "author": {"id": "author-id", "future": true},
            "future": true
        },
        "op_user": {
            "id": "operator-id",
            "username": "operator",
            "avatar": "https://example.com/avatar.png",
            "bot": false,
            "future": true
        },
        "future": true
    })
}

#[test]
fn decodes_and_validates_message_delete_notices() {
    let event: MessageDeleteEvent = serde_json::from_value(deleted_message()).unwrap();
    event.validate().unwrap();
    assert_eq!(event.message.id, "message-id");
    assert_eq!(event.op_user.id.as_deref(), Some("operator-id"));
    assert_eq!(event.op_user.bot, Some(false));

    for (path, expected_field) in [
        (["message", "id"], "message.id"),
        (["message", "guild_id"], "message.guild_id"),
        (["message", "channel_id"], "message.channel_id"),
        (["op_user", "id"], "op_user.id"),
    ] {
        let mut invalid = deleted_message();
        invalid[path[0]][path[1]] = json!(" ");
        assert_eq!(
            serde_json::from_value::<MessageDeleteEvent>(invalid)
                .unwrap()
                .validate()
                .unwrap_err(),
            NoticeValidationError::EmptyField {
                field: expected_field
            }
        );
    }
}

#[test]
fn decodes_and_validates_message_audit_notices() {
    let event: MessageAuditEvent = serde_json::from_value(json!({
        "audit_id": "audit-id",
        "message_id": "message-id",
        "guild_id": "guild-id",
        "channel_id": "channel-id",
        "audit_time": "2026-08-22T10:01:00+08:00",
        "create_time": "2026-08-22T10:00:00+08:00",
        "future": true
    }))
    .unwrap();
    event.validate(MessageAuditOutcome::Pass).unwrap();
    assert_eq!(event.seq_in_channel, None);

    let invalid = MessageAuditEvent {
        audit_id: " ".to_owned(),
        ..event.clone()
    };
    assert_eq!(
        invalid.validate(MessageAuditOutcome::Pass).unwrap_err(),
        NoticeValidationError::EmptyField { field: "audit_id" }
    );
    let rejected: MessageAuditEvent = serde_json::from_value(json!({
        "audit_id": "audit-id",
        "guild_id": "guild-id",
        "channel_id": "channel-id",
        "audit_time": "2026-08-22T10:01:00+08:00",
        "create_time": "2026-08-22T10:00:00+08:00"
    }))
    .unwrap();
    rejected.validate(MessageAuditOutcome::Reject).unwrap();
    let rejected_empty_message_id = MessageAuditEvent {
        message_id: Some(String::new()),
        ..rejected.clone()
    };
    rejected_empty_message_id
        .validate(MessageAuditOutcome::Reject)
        .unwrap();
    assert_eq!(
        rejected_empty_message_id
            .validate(MessageAuditOutcome::Pass)
            .unwrap_err(),
        NoticeValidationError::EmptyField {
            field: "message_id"
        }
    );
    let rejected_with_message_id = MessageAuditEvent {
        message_id: Some("message-id".to_owned()),
        ..rejected.clone()
    };
    rejected_with_message_id
        .validate(MessageAuditOutcome::Reject)
        .unwrap();

    for field in ["audit_time", "create_time"] {
        let mut invalid = event.clone();
        match field {
            "audit_time" => invalid.audit_time = "not-a-timestamp".to_owned(),
            "create_time" => invalid.create_time = "not-a-timestamp".to_owned(),
            _ => unreachable!(),
        }
        assert_eq!(
            invalid.validate(MessageAuditOutcome::Pass).unwrap_err(),
            NoticeValidationError::InvalidTimestamp { field }
        );
    }
}

#[test]
fn decodes_and_validates_subscription_status_notices() {
    let direct: SubscribeMessageStatusEvent = serde_json::from_value(json!({
        "openid": "user-openid",
        "result": [{
            "template_id": 10001,
            "custom_template_id": "template-custom",
            "op": 1,
            "subscribe_id": "subscribe-id",
            "subscribe_ts": 1_784_276_815,
            "update_ts": 1_784_276_820,
            "future": true
        }],
        "future": true
    }))
    .unwrap();
    direct.validate().unwrap();

    let group: SubscribeMessageStatusEvent = serde_json::from_value(json!({
        "group_openid": "group-openid",
        "result": [{
            "template_id": 10002,
            "custom_template_id": "template-custom",
            "op": 2,
            "subscribe_id": "subscribe-id",
            "subscribe_ts": 1_784_276_815,
            "update_ts": 1_784_276_820
        }]
    }))
    .unwrap();
    group.validate().unwrap();

    let missing_target: SubscribeMessageStatusEvent = serde_json::from_value(json!({
        "result": []
    }))
    .unwrap();
    assert_eq!(
        missing_target.validate().unwrap_err(),
        NoticeValidationError::MissingSubscriptionTarget
    );

    for (field, expected_field) in [
        ("openid", "openid"),
        ("custom_template_id", "result.custom_template_id"),
        ("subscribe_id", "result.subscribe_id"),
    ] {
        let mut invalid = serde_json::to_value(&direct).unwrap();
        if field == "openid" {
            invalid["group_openid"] = json!("group-openid");
            invalid[field] = json!(" ");
        } else {
            invalid["result"][0][field] = json!(" ");
        }
        assert_eq!(
            serde_json::from_value::<SubscribeMessageStatusEvent>(invalid)
                .unwrap()
                .validate()
                .unwrap_err(),
            NoticeValidationError::EmptyField {
                field: expected_field
            }
        );
    }

    let mut future_operation = direct;
    future_operation.result[0].op = qqbot_protocol::SubscriptionOperation::new(3);
    future_operation.validate().unwrap();
    assert_eq!(future_operation.result[0].op.value(), 3);

    assert!(
        serde_json::from_value::<SubscribeMessageStatusEvent>(json!({
            "openid": "user-openid",
            "result": [{
                "template_id": 10001,
                "custom_template_id": "template-custom",
                "op": 1,
                "subscribe_id": "subscribe-id",
                "update_ts": 1_784_276_820
            }]
        }))
        .is_err()
    );
}
