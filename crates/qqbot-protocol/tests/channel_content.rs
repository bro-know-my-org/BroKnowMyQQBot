use qqbot_protocol::{
    AnnouncementType, ChannelContentValidationError, CreateGuildAnnouncementRequest,
    CreateSchedule, CreateScheduleRequest, EpochMillis, RecommendChannel, Schedule,
    ScheduleRemindType, UpdateSchedule, UpdateScheduleRequest,
};
use serde_json::{Value, json};

fn millis(value: u64) -> EpochMillis {
    EpochMillis::new(value.to_string()).unwrap()
}

fn schedule() -> CreateSchedule {
    CreateSchedule {
        name: "频道活动".to_owned(),
        description: Some("一起参加".to_owned()),
        start_timestamp: millis(1_784_279_600_000),
        end_timestamp: millis(1_784_283_200_000),
        jump_channel_id: Some("channel-id".to_owned()),
        remind_type: ScheduleRemindType::FIFTEEN_MINUTES,
    }
}

#[test]
fn validates_message_announcement_shape() {
    let message = CreateGuildAnnouncementRequest::message("message-id", "channel-id");
    message.validate().unwrap();
    assert_eq!(
        serde_json::to_value(&message).unwrap(),
        json!({
            "message_id":"message-id",
            "channel_id":"channel-id",
            "announces_type":0
        })
    );

    let mut invalid = message.clone();
    invalid.recommend_channels = Some(vec![RecommendChannel {
        channel_id: "channel-id".to_owned(),
        introduce: "介绍".to_owned(),
    }]);
    assert_eq!(
        invalid.validate().unwrap_err(),
        ChannelContentValidationError::InvalidAnnouncementShape
    );
    let mut compatible = message.clone();
    compatible.recommend_channels = Some(Vec::new());
    compatible.validate().unwrap();
    assert_eq!(
        serde_json::to_value(compatible).unwrap()["recommend_channels"],
        json!([])
    );

    for (field, message_id, channel_id) in [
        ("message_id", None, Some("channel-id".to_owned())),
        (
            "message_id",
            Some(" ".to_owned()),
            Some("channel-id".to_owned()),
        ),
        ("channel_id", Some("message-id".to_owned()), None),
        (
            "channel_id",
            Some("message-id".to_owned()),
            Some(" ".to_owned()),
        ),
    ] {
        let invalid = CreateGuildAnnouncementRequest {
            message_id,
            channel_id,
            announces_type: AnnouncementType::MEMBER,
            recommend_channels: None,
        };
        assert_eq!(
            invalid.validate().unwrap_err(),
            ChannelContentValidationError::EmptyField { field }
        );
    }

    let invalid = CreateGuildAnnouncementRequest {
        announces_type: AnnouncementType::new(2),
        ..message
    };
    assert_eq!(
        invalid.validate().unwrap_err(),
        ChannelContentValidationError::InvalidAnnouncementType { value: 2 }
    );
}

#[test]
fn validates_recommended_channel_announcement_shape() {
    let recommended = CreateGuildAnnouncementRequest::recommended(vec![RecommendChannel {
        channel_id: "channel-id".to_owned(),
        introduce: "欢迎来这里".to_owned(),
    }]);
    recommended.validate().unwrap();
    assert_eq!(
        serde_json::to_value(&recommended).unwrap(),
        json!({
            "announces_type":1,
            "recommend_channels":[{
                "channel_id":"channel-id",
                "introduce":"欢迎来这里"
            }]
        })
    );

    let mut invalid = recommended.clone();
    invalid.message_id = Some("message-id".to_owned());
    assert_eq!(
        invalid.validate().unwrap_err(),
        ChannelContentValidationError::InvalidAnnouncementShape
    );
    let mut compatible = recommended.clone();
    compatible.message_id = Some(String::new());
    compatible.channel_id = Some(String::new());
    compatible.validate().unwrap();
    let encoded = serde_json::to_value(compatible).unwrap();
    assert_eq!(encoded["message_id"], "");
    assert_eq!(encoded["channel_id"], "");

    for count in [0, 4] {
        let invalid = CreateGuildAnnouncementRequest::recommended(
            (0..count)
                .map(|index| RecommendChannel {
                    channel_id: format!("channel-{index}"),
                    introduce: "介绍".to_owned(),
                })
                .collect(),
        );
        assert_eq!(
            invalid.validate().unwrap_err(),
            ChannelContentValidationError::RecommendedChannelCount { count }
        );
    }

    for (field, channel_id, introduce) in [
        ("channel_id", " ", "介绍"),
        ("introduce", "channel-id", " "),
    ] {
        let invalid = CreateGuildAnnouncementRequest::recommended(vec![RecommendChannel {
            channel_id: channel_id.to_owned(),
            introduce: introduce.to_owned(),
        }]);
        assert_eq!(
            invalid.validate().unwrap_err(),
            ChannelContentValidationError::InvalidRecommendedChannel { index: 0, field }
        );
    }
}

#[test]
fn schedule_wire_types_preserve_millisecond_strings() {
    let request = CreateScheduleRequest {
        schedule: schedule(),
    };
    request.validate().unwrap();
    let encoded = serde_json::to_value(&request).unwrap();
    assert_eq!(
        encoded["schedule"]["start_timestamp"],
        json!("1784279600000")
    );
    assert_eq!(encoded["schedule"]["remind_type"], json!("3"));

    assert!(serde_json::from_value::<EpochMillis>(json!(1_784_279_600_000_u64)).is_err());
    assert!(serde_json::from_value::<EpochMillis>(json!("not-millis")).is_err());
    assert!(serde_json::from_value::<EpochMillis>(json!("+123")).is_err());
    assert!(EpochMillis::new("+123").is_err());
    assert!(EpochMillis::new("").is_err());
    assert!(EpochMillis::new("１２３").is_err());
    assert_eq!(
        serde_json::to_value(EpochMillis::new("00123").unwrap()).unwrap(),
        json!("00123")
    );
    assert!(EpochMillis::new(u64::MAX.to_string()).is_ok());
    assert!(EpochMillis::new("18446744073709551616").is_err());
    assert!(serde_json::from_value::<ScheduleRemindType>(json!(3)).is_err());
    assert!(serde_json::from_value::<ScheduleRemindType>(json!("+1")).is_err());

    let response: Schedule = serde_json::from_value(json!({
        "id":"schedule-id",
        "name":"频道活动",
        "start_timestamp":"1784279600000",
        "end_timestamp":"1784283200000",
        "remind_type":"0",
        "future":true
    }))
    .unwrap();
    assert_eq!(response.start_timestamp.value(), 1_784_279_600_000);
    assert_eq!(response.description, None);
    assert_eq!(response.creator, None);
    assert_eq!(response.jump_channel_id, None);
}

#[test]
fn validates_create_schedule_requests() {
    let mut invalid = schedule();
    invalid.name = " ".to_owned();
    assert_eq!(
        invalid.validate().unwrap_err(),
        ChannelContentValidationError::EmptyField {
            field: "schedule.name"
        }
    );

    let mut invalid = schedule();
    invalid.end_timestamp = millis(invalid.start_timestamp.value() - 1);
    assert_eq!(
        invalid.validate().unwrap_err(),
        ChannelContentValidationError::ScheduleEndsBeforeStart
    );

    let mut invalid = schedule();
    invalid.end_timestamp = millis(invalid.start_timestamp.value() + 7 * 24 * 60 * 60 * 1_000 + 1);
    assert_eq!(
        invalid.validate().unwrap_err(),
        ChannelContentValidationError::ScheduleDurationTooLong
    );

    assert_eq!(
        ScheduleRemindType::new(6).unwrap_err(),
        ChannelContentValidationError::InvalidRemindType { value: 6 }
    );
    assert_eq!(
        serde_json::from_value::<ScheduleRemindType>(json!("6"))
            .unwrap()
            .value(),
        6
    );
    assert_eq!(
        serde_json::from_value::<ScheduleRemindType>(json!("256"))
            .unwrap()
            .value(),
        256
    );

    let malformed: Result<CreateScheduleRequest, _> = serde_json::from_value(json!({
        "schedule": {
            "name":"活动",
            "start_timestamp":"bad",
            "end_timestamp":"1784283200000",
            "remind_type":"0"
        }
    }));
    assert!(malformed.is_err());

    let unsupported_reminder: CreateScheduleRequest = serde_json::from_value(json!({
        "schedule": {
            "name":"活动",
            "start_timestamp":"1784279600000",
            "end_timestamp":"1784283200000",
            "remind_type":"6"
        }
    }))
    .unwrap();
    assert_eq!(
        unsupported_reminder.validate().unwrap_err(),
        ChannelContentValidationError::InvalidRemindType { value: 6 }
    );

    assert_eq!(
        serde_json::to_value(ScheduleRemindType::AT_START).unwrap(),
        Value::String("1".to_owned())
    );
}

#[test]
fn validates_partial_schedule_updates() {
    let empty = UpdateScheduleRequest {
        schedule: UpdateSchedule::default(),
    };
    assert_eq!(
        empty.validate().unwrap_err(),
        ChannelContentValidationError::EmptyScheduleUpdate
    );

    let partial = UpdateScheduleRequest {
        schedule: UpdateSchedule {
            description: Some(String::new()),
            ..UpdateSchedule::default()
        },
    };
    partial.validate().unwrap();
    assert_eq!(
        serde_json::to_value(partial).unwrap(),
        json!({"schedule":{"description":""}})
    );

    let unsupported_reminder = serde_json::from_value(json!("6")).unwrap();
    for (update, expected) in [
        (
            UpdateSchedule {
                remind_type: Some(unsupported_reminder),
                ..UpdateSchedule::default()
            },
            ChannelContentValidationError::InvalidRemindType { value: 6 },
        ),
        (
            UpdateSchedule {
                name: Some(" ".to_owned()),
                ..UpdateSchedule::default()
            },
            ChannelContentValidationError::EmptyField {
                field: "schedule.name",
            },
        ),
        (
            UpdateSchedule {
                jump_channel_id: Some(" ".to_owned()),
                ..UpdateSchedule::default()
            },
            ChannelContentValidationError::EmptyField {
                field: "schedule.jump_channel_id",
            },
        ),
        (
            UpdateSchedule {
                start_timestamp: Some(millis(2_000)),
                end_timestamp: Some(millis(1_000)),
                ..UpdateSchedule::default()
            },
            ChannelContentValidationError::ScheduleEndsBeforeStart,
        ),
        (
            UpdateSchedule {
                start_timestamp: Some(millis(1_000)),
                end_timestamp: Some(millis(1_000 + 7 * 24 * 60 * 60 * 1_000 + 1)),
                ..UpdateSchedule::default()
            },
            ChannelContentValidationError::ScheduleDurationTooLong,
        ),
    ] {
        assert_eq!(update.validate().unwrap_err(), expected);
    }
}
