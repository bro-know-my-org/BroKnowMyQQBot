use qqbot_protocol::{
    AudioActionEvent, AudioControlRequest, AudioOrLiveChannelMemberEvent, AudioOrLiveChannelType,
    AudioStatus, AudioValidationError, CreateForumThreadRequest, ForumAuditResult, ForumAuditType,
    ForumContent, ForumCreateTime, ForumFormat, ForumPostEvent, ForumPublishAuditEvent,
    ForumReplyEvent, ForumThread, ForumThreadDetail, ForumThreadList, ForumValidationError,
    OpenForumEvent,
};
use serde_json::{Value, json};

#[test]
fn validates_audio_control_states_and_dispatches() {
    for request in [
        AudioControlRequest::start("https://example.com/audio.mp3", Some("播放中".to_owned())),
        AudioControlRequest::pause(),
        AudioControlRequest::resume(),
        AudioControlRequest::stop(),
    ] {
        request.validate().unwrap();
    }

    for (request, expected) in [
        (
            AudioControlRequest {
                audio_url: None,
                text: None,
                status: AudioStatus::START,
            },
            AudioValidationError::MissingAudioUrl,
        ),
        (
            AudioControlRequest {
                audio_url: Some("https://example.com/audio.mp3".to_owned()),
                text: None,
                status: AudioStatus::PAUSE,
            },
            AudioValidationError::UnexpectedPlaybackField { field: "audio_url" },
        ),
        (
            AudioControlRequest {
                audio_url: None,
                text: None,
                status: AudioStatus::new(4),
            },
            AudioValidationError::InvalidStatus { value: 4 },
        ),
        (
            AudioControlRequest::start("file:///tmp/audio.mp3", None),
            AudioValidationError::InvalidAudioUrl,
        ),
        (
            AudioControlRequest::start("https://example.com/has space", None),
            AudioValidationError::InvalidAudioUrl,
        ),
    ] {
        assert_eq!(request.validate().unwrap_err(), expected);
    }

    let action: AudioActionEvent = serde_json::from_value(json!({
        "guild_id":"guild-id","channel_id":"channel-id","audio_url":"","text":"",
        "future":true
    }))
    .unwrap();
    action.validate().unwrap();

    let member: AudioOrLiveChannelMemberEvent = serde_json::from_value(json!({
        "guild_id":"guild-id","channel_id":"channel-id","channel_type":7,
        "user_id":"user-id","future":true
    }))
    .unwrap();
    member.validate().unwrap();
    assert_eq!(member.channel_type.value(), 7);
    assert_eq!(AudioOrLiveChannelType::AUDIO.value(), 2);
    assert_eq!(AudioOrLiveChannelType::LIVE.value(), 5);
}

#[test]
fn validates_forum_create_requests_and_async_task_time() {
    for format in [
        ForumFormat::TEXT,
        ForumFormat::HTML,
        ForumFormat::MARKDOWN,
        ForumFormat::JSON,
    ] {
        let content = if format == ForumFormat::JSON {
            r#"{"paragraphs":[]}"#
        } else {
            "content"
        };
        CreateForumThreadRequest {
            title: "标题".to_owned(),
            content: content.to_owned(),
            format,
        }
        .validate()
        .unwrap();
    }

    for (request, expected) in [
        (
            CreateForumThreadRequest {
                title: " ".to_owned(),
                content: "content".to_owned(),
                format: ForumFormat::TEXT,
            },
            ForumValidationError::EmptyField { field: "title" },
        ),
        (
            CreateForumThreadRequest {
                title: "title".to_owned(),
                content: " ".to_owned(),
                format: ForumFormat::TEXT,
            },
            ForumValidationError::EmptyField { field: "content" },
        ),
        (
            CreateForumThreadRequest {
                title: "title".to_owned(),
                content: "not-json".to_owned(),
                format: ForumFormat::JSON,
            },
            ForumValidationError::InvalidJsonContent,
        ),
        (
            CreateForumThreadRequest {
                title: "title".to_owned(),
                content: "content".to_owned(),
                format: ForumFormat::new(5),
            },
            ForumValidationError::InvalidFormat { value: 5 },
        ),
    ] {
        assert_eq!(request.validate().unwrap_err(), expected);
    }

    let seconds = ForumCreateTime::new("1645503180").unwrap();
    assert_eq!(seconds.value(), 1_645_503_180);
    assert_eq!(serde_json::to_value(seconds).unwrap(), json!("1645503180"));
    for invalid in ["", "+1", "１２３", "18446744073709551616"] {
        assert!(ForumCreateTime::new(invalid).is_err());
    }
    assert!(serde_json::from_value::<ForumCreateTime>(json!(1_645_503_180)).is_err());
}

#[test]
fn decodes_forum_responses_and_both_content_wire_shapes() {
    let list: ForumThreadList = serde_json::from_value(json!({
        "threads":[{
            "guild_id":"guild-id","channel_id":"channel-id","author_id":"author-id",
            "thread_info":{
                "thread_id":"thread-string","title":"JSON string title",
                "content":"{\"paragraphs\":[]}",
                "date_time":"2026-08-22T10:00:00+08:00"
            }
        },{
            "guild_id":"guild-id","channel_id":"channel-id","author_id":"author-id",
            "thread_info":{
                "thread_id":"thread-expanded","title":[{"type":1,"text":"title"}],
                "content":{"paragraphs":[]},
                "date_time":"2026-08-22T10:01:00+08:00"
            }
        }],
        "is_finish":1
    }))
    .unwrap();
    assert_eq!(list.is_finish.value(), 1);
    assert_eq!(
        list.threads[0].thread_info.title.as_str(),
        Some("JSON string title")
    );
    assert!(list.threads[1].thread_info.title.as_value().is_array());
    assert!(list.threads[1].thread_info.content.as_value().is_object());
    assert!(
        serde_json::from_value::<ForumThreadList>(json!({"is_finish":1})).is_err(),
        "threads must be explicitly present in forum list responses"
    );
    for thread in &list.threads {
        thread.validate().unwrap();
    }

    let detail: ForumThreadDetail = serde_json::from_value(json!({
        "thread":serde_json::to_value(&list.threads[0]).unwrap(),
        "future":true
    }))
    .unwrap();
    assert_eq!(detail.thread.thread_info.thread_id, "thread-string");

    for invalid in [json!(null), json!(true), json!(42)] {
        assert!(serde_json::from_value::<ForumContent>(invalid).is_err());
    }
    assert_eq!(
        ForumContent::try_from(json!(false)).unwrap_err(),
        ForumValidationError::InvalidContentShape
    );

    let mut invalid_list = list.clone();
    invalid_list.threads[0].thread_info.date_time = "not-time".to_owned();
    assert_eq!(
        invalid_list.validate().unwrap_err(),
        ForumValidationError::InvalidTimestamp {
            field: "thread_info.date_time"
        }
    );
    let mut invalid_finish = list.clone();
    invalid_finish.is_finish = qqbot_protocol::ForumListFinish::new(2);
    assert_eq!(
        invalid_finish.validate().unwrap_err(),
        ForumValidationError::InvalidListFinish { value: 2 }
    );
    let mut invalid_detail = detail;
    invalid_detail.thread.thread_info.date_time = "not-time".to_owned();
    assert!(matches!(
        invalid_detail.validate(),
        Err(ForumValidationError::InvalidTimestamp { .. })
    ));
}

#[test]
fn validates_all_forum_dispatch_payload_families() {
    let thread: ForumThread = serde_json::from_value(json!({
        "guild_id":"guild-id","channel_id":"channel-id","author_id":"author-id",
        "thread_info":{
            "thread_id":"thread-id","title":[],"content":"content",
            "date_time":"2026-08-22T10:00:00+08:00"
        }
    }))
    .unwrap();
    thread.validate().unwrap();

    let post: ForumPostEvent = serde_json::from_value(json!({
        "guild_id":"guild-id","channel_id":"channel-id","author_id":"author-id",
        "post_info":{
            "thread_id":"thread-id","post_id":"post-id","content":[],
            "date_time":"2026-08-22T10:01:00+08:00"
        }
    }))
    .unwrap();
    post.validate().unwrap();

    let reply: ForumReplyEvent = serde_json::from_value(json!({
        "guild_id":"guild-id","channel_id":"channel-id","author_id":"author-id",
        "reply_info":{
            "thread_id":"thread-id","post_id":"post-id","reply_id":"reply-id",
            "content":{},"date_time":"2026-08-22T10:02:00+08:00"
        }
    }))
    .unwrap();
    reply.validate().unwrap();

    let audit: ForumPublishAuditEvent = serde_json::from_value(json!({
        "guild_id":"guild-id","channel_id":"channel-id","author_id":"author-id",
        "thread_id":"thread-id","post_id":"","reply_id":"","type":9,"result":8,
        "err_msg":"future result","task_id":"task-id",
        "date_time":"2026-08-22T10:03:00+08:00","future":true
    }))
    .unwrap();
    audit.validate().unwrap();
    assert_eq!(audit.audit_type.value(), 9);
    assert_eq!(audit.result.value(), 8);
    assert_eq!(ForumAuditType::THREAD.value(), 1);
    assert_eq!(ForumAuditResult::SUCCESS.value(), 0);

    let mut invalid_audit = audit.clone();
    invalid_audit.audit_type = ForumAuditType::POST;
    assert_eq!(
        invalid_audit.validate().unwrap_err(),
        ForumValidationError::EmptyField { field: "post_id" }
    );

    let open: OpenForumEvent = serde_json::from_value(json!({
        "guild_id":"guild-id","channel_id":"channel-id","author_id":"author-id",
        "future":true
    }))
    .unwrap();
    open.validate().unwrap();

    let mut invalid = thread;
    invalid.thread_info.date_time = "not-time".to_owned();
    assert_eq!(
        invalid.validate().unwrap_err(),
        ForumValidationError::InvalidTimestamp {
            field: "thread_info.date_time"
        }
    );

    assert_eq!(
        serde_json::to_value(ForumAuditType::POST).unwrap(),
        Value::from(2)
    );
}
