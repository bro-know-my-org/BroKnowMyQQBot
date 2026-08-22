//! QQ dispatch to platform-independent event mapping.

use std::fmt::Write as _;

use bot_core::{
    AdapterId, CommonMessage, Event, EventEnvelope, EventId, MessageSegment, MessageTarget, Sender,
};
use chrono::{DateTime, Utc};
use qqbot_protocol::{
    AudioActionEvent, AudioOrLiveChannelMemberEvent, AudioValidationError, C2cMessageStatusEvent,
    ChannelEvent, ForumPostEvent, ForumPublishAuditEvent, ForumReplyEvent, ForumThread,
    ForumValidationError, FriendAddEvent, FriendDeleteEvent, GatewayPayload, GroupJoinRequestEvent,
    GroupMemberEvent, GroupMemberEventValidationError, GroupMessageStatusEvent, GroupRobotEvent,
    GuildDispatchValidationError, GuildEvent, GuildMemberEvent, InteractionEvent,
    InteractionValidationError, MessageAuditEvent, MessageAuditOutcome, MessageDeleteEvent,
    MessageReactionEvent, NoticeValidationError, OpenForumEvent, QqMessage,
    ReactionValidationError, SubscribeMessageStatusEvent,
};
use serde::Deserialize as _;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

const GROUP_EVENTS: &[&str] = &["GROUP_AT_MESSAGE_CREATE", "GROUP_MESSAGE_CREATE"];
const PRIVATE_EVENTS: &[&str] = &["C2C_MESSAGE_CREATE"];
const CHANNEL_EVENTS: &[&str] = &["AT_MESSAGE_CREATE", "MESSAGE_CREATE"];
const GUILD_DIRECT_EVENTS: &[&str] = &["DIRECT_MESSAGE_CREATE"];
const REQUEST_EVENTS: &[&str] = &["GROUP_JOIN_REQUEST"];

#[derive(Debug, Error)]
pub(crate) enum MappingError {
    #[error("QQ dispatch `{event_type}` could not be decoded")]
    Decode {
        event_type: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("QQ dispatch `{event_type}` is missing {field}")]
    MissingField {
        event_type: String,
        field: &'static str,
    },
    #[error("QQ dispatch `{event_type}` has an invalid timestamp")]
    InvalidTimestamp {
        event_type: String,
        #[source]
        source: chrono::ParseError,
    },
    #[error("QQ dispatch `{event_type}` has invalid Unix timestamp {timestamp}")]
    InvalidUnixTimestamp { event_type: String, timestamp: u64 },
    #[error("QQ structured dispatch `{event_type}` contains invalid data")]
    InvalidEventData { event_type: String },
    #[error("QQ reaction dispatch `{event_type}` contains invalid data")]
    InvalidReaction {
        event_type: String,
        #[source]
        source: ReactionValidationError,
    },
    #[error("QQ group member dispatch `{event_type}` contains invalid data")]
    InvalidGroupMember {
        event_type: String,
        #[source]
        source: GroupMemberEventValidationError,
    },
    #[error("QQ guild dispatch `{event_type}` contains invalid data")]
    InvalidGuildDispatch {
        event_type: String,
        #[source]
        source: GuildDispatchValidationError,
    },
    #[error("QQ interaction dispatch `{event_type}` contains invalid data")]
    InvalidInteraction {
        event_type: String,
        #[source]
        source: InteractionValidationError,
    },
    #[error("QQ notice dispatch `{event_type}` contains invalid data")]
    InvalidNotice {
        event_type: String,
        #[source]
        source: NoticeValidationError,
    },
    #[error("QQ audio dispatch `{event_type}` contains invalid data")]
    InvalidAudio {
        event_type: String,
        #[source]
        source: AudioValidationError,
    },
    #[error("QQ forum dispatch `{event_type}` contains invalid data")]
    InvalidForum {
        event_type: String,
        #[source]
        source: ForumValidationError,
    },
}

pub(crate) fn map_dispatch(
    adapter: &AdapterId,
    payload: &GatewayPayload,
) -> Result<Option<EventEnvelope>, MappingError> {
    let Some(event_type) = payload.t.as_deref() else {
        return Ok(None);
    };
    if let Some(mapped) = map_typed_notice(adapter, payload, event_type) {
        return mapped.map(Some);
    }
    if event_type == "INTERACTION_CREATE" {
        return map_interaction_event(adapter, payload, event_type).map(Some);
    }
    if REQUEST_EVENTS.contains(&event_type) {
        return map_group_join_request(adapter, payload, event_type).map(Some);
    }
    if !GROUP_EVENTS.contains(&event_type)
        && !PRIVATE_EVENTS.contains(&event_type)
        && !CHANNEL_EVENTS.contains(&event_type)
        && !GUILD_DIRECT_EVENTS.contains(&event_type)
    {
        return Ok(None);
    }

    let message: QqMessage =
        serde_json::from_value(payload.d.clone()).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    let target = if GROUP_EVENTS.contains(&event_type) {
        MessageTarget::Group {
            group_id: required(message.group_openid.clone(), event_type, "group_openid")?,
        }
    } else if PRIVATE_EVENTS.contains(&event_type) {
        MessageTarget::Private {
            user_id: required(
                message.author.user_openid.clone(),
                event_type,
                "author.user_openid",
            )?,
        }
    } else if CHANNEL_EVENTS.contains(&event_type) {
        MessageTarget::Channel {
            channel_id: required(message.channel_id.clone(), event_type, "channel_id")?,
        }
    } else {
        MessageTarget::GuildDirect {
            guild_id: required(message.guild_id.clone(), event_type, "guild_id")?,
        }
    };
    let sender_id = [
        message.author.member_openid.as_deref(),
        message.author.user_openid.as_deref(),
        message.author.id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find(|value| !value.trim().is_empty())
    .map(str::to_owned)
    .ok_or_else(|| MappingError::MissingField {
        event_type: event_type.to_owned(),
        field: "author identifier",
    })?;
    let message_id = required(Some(message.id), event_type, "message.id")?;
    let event_id = payload
        .id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| message_id.clone());
    let timestamp = message
        .timestamp
        .as_deref()
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|source| MappingError::InvalidTimestamp {
            event_type: event_type.to_owned(),
            source,
        })?
        .map(|value| value.with_timezone(&Utc));
    let text = message.content.clone();
    let raw = serde_json::to_value(payload).unwrap_or_else(|_| payload.d.clone());

    Ok(Some(EventEnvelope {
        id: EventId::new(event_id),
        adapter: adapter.clone(),
        delivery_id: None,
        timestamp,
        event: Event::Message(CommonMessage {
            message_id,
            target,
            sender: Sender {
                id: sender_id,
                display_name: message.author.username,
            },
            text: text.clone(),
            segments: vec![MessageSegment::Text { text }],
            reply_to: None,
        }),
        raw,
    }))
}

fn map_typed_notice(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Option<Result<EventEnvelope, MappingError>> {
    match event_type {
        "GUILD_CREATE" | "GUILD_UPDATE" | "GUILD_DELETE" => {
            Some(map_guild_event(adapter, payload, event_type))
        }
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE" => {
            Some(map_channel_event(adapter, payload, event_type))
        }
        "GUILD_MEMBER_ADD" | "GUILD_MEMBER_UPDATE" | "GUILD_MEMBER_REMOVE" => {
            Some(map_guild_member_event(adapter, payload, event_type))
        }
        "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
            Some(map_message_reaction(adapter, payload, event_type))
        }
        "GROUP_MEMBER_ADD" | "GROUP_MEMBER_REMOVE" => {
            Some(map_group_member(adapter, payload, event_type))
        }
        "MESSAGE_DELETE" | "PUBLIC_MESSAGE_DELETE" | "DIRECT_MESSAGE_DELETE" => {
            Some(map_message_delete(adapter, payload, event_type))
        }
        "SUBSCRIBE_MESSAGE_STATUS" => Some(map_subscription_status(adapter, payload, event_type)),
        "MESSAGE_AUDIT_PASS" | "MESSAGE_AUDIT_REJECT" => {
            Some(map_message_audit(adapter, payload, event_type))
        }
        "FRIEND_ADD" => Some(map_friend_add(adapter, payload, event_type)),
        "FRIEND_DEL" => Some(map_friend_delete(adapter, payload, event_type)),
        "GROUP_ADD_ROBOT" | "GROUP_DEL_ROBOT" => {
            Some(map_group_robot(adapter, payload, event_type))
        }
        "GROUP_MSG_RECEIVE" | "GROUP_MSG_REJECT" => {
            Some(map_group_message_status(adapter, payload, event_type))
        }
        "C2C_MSG_RECEIVE" | "C2C_MSG_REJECT" => {
            Some(map_c2c_message_status(adapter, payload, event_type))
        }
        "AUDIO_START" | "AUDIO_FINISH" | "AUDIO_ON_MIC" | "AUDIO_OFF_MIC" => {
            Some(map_audio_event(adapter, payload, event_type))
        }
        "FORUM_THREAD_CREATE" | "FORUM_THREAD_UPDATE" | "FORUM_THREAD_DELETE" => {
            Some(map_forum_thread_event(adapter, payload, event_type))
        }
        "FORUM_POST_CREATE" | "FORUM_POST_DELETE" => {
            Some(map_forum_post_event(adapter, payload, event_type))
        }
        "FORUM_REPLY_CREATE" | "FORUM_REPLY_DELETE" => {
            Some(map_forum_reply_event(adapter, payload, event_type))
        }
        "FORUM_PUBLISH_AUDIT_RESULT" => Some(map_forum_audit_event(adapter, payload, event_type)),
        "OPEN_FORUM_THREAD_CREATE"
        | "OPEN_FORUM_THREAD_UPDATE"
        | "OPEN_FORUM_THREAD_DELETE"
        | "OPEN_FORUM_POST_CREATE"
        | "OPEN_FORUM_POST_DELETE"
        | "OPEN_FORUM_REPLY_CREATE"
        | "OPEN_FORUM_REPLY_DELETE" => Some(map_open_forum_event(adapter, payload, event_type)),
        "AUDIO_OR_LIVE_CHANNEL_MEMBER_ENTER" | "AUDIO_OR_LIVE_CHANNEL_MEMBER_EXIT" => {
            Some(map_audio_live_member_event(adapter, payload, event_type))
        }
        _ => None,
    }
}

fn map_friend_add(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event = FriendAddEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
        event_type: event_type.to_owned(),
        source,
    })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidNotice {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_unix_notice(adapter, payload, event_type, event.timestamp)
}

fn map_friend_delete(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        FriendDeleteEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidNotice {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_unix_notice(adapter, payload, event_type, event.timestamp)
}

fn map_group_robot(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        GroupRobotEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidNotice {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_unix_notice(adapter, payload, event_type, event.timestamp)
}

fn map_group_message_status(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event = GroupMessageStatusEvent::deserialize(&payload.d).map_err(|source| {
        MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        }
    })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidNotice {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_unix_notice(adapter, payload, event_type, event.timestamp)
}

fn map_c2c_message_status(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        C2cMessageStatusEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidNotice {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_unix_notice(adapter, payload, event_type, event.timestamp)
}

fn map_unix_notice(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
    timestamp: u64,
) -> Result<EventEnvelope, MappingError> {
    let timestamp = unix_timestamp(timestamp, event_type)?;
    let mut envelope = map_structured_event(adapter, payload, event_type, None, Event::Notice)?;
    envelope.timestamp = Some(timestamp);
    Ok(envelope)
}

fn map_audio_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        AudioActionEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidAudio {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_structured_event(adapter, payload, event_type, None, Event::Notice)
}

fn map_audio_live_member_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event = AudioOrLiveChannelMemberEvent::deserialize(&payload.d).map_err(|source| {
        MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        }
    })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidAudio {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_structured_event(adapter, payload, event_type, None, Event::Notice)
}

fn map_open_forum_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event = OpenForumEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
        event_type: event_type.to_owned(),
        source,
    })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidForum {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_structured_event(adapter, payload, event_type, None, Event::Notice)
}

fn map_forum_thread_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event = ForumThread::deserialize(&payload.d).map_err(|source| MappingError::Decode {
        event_type: event_type.to_owned(),
        source,
    })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidForum {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_forum_with_timestamp(adapter, payload, event_type, &event.thread_info.date_time)
}

fn map_forum_post_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event = ForumPostEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
        event_type: event_type.to_owned(),
        source,
    })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidForum {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_forum_with_timestamp(adapter, payload, event_type, &event.post_info.date_time)
}

fn map_forum_reply_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        ForumReplyEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidForum {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_forum_with_timestamp(adapter, payload, event_type, &event.reply_info.date_time)
}

fn map_forum_audit_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        ForumPublishAuditEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidForum {
            event_type: event_type.to_owned(),
            source,
        })?;
    let mut envelope = map_structured_event(adapter, payload, event_type, None, Event::Notice)?;
    envelope.timestamp = event
        .date_time
        .as_deref()
        .map(DateTime::parse_from_rfc3339)
        .transpose()
        .map_err(|source| MappingError::InvalidTimestamp {
            event_type: event_type.to_owned(),
            source,
        })?
        .map(|value| value.with_timezone(&Utc));
    Ok(envelope)
}

fn map_forum_with_timestamp(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
    date_time: &str,
) -> Result<EventEnvelope, MappingError> {
    let mut envelope = map_structured_event(adapter, payload, event_type, None, Event::Notice)?;
    envelope.timestamp = Some(
        DateTime::parse_from_rfc3339(date_time)
            .map_err(|source| MappingError::InvalidTimestamp {
                event_type: event_type.to_owned(),
                source,
            })?
            .with_timezone(&Utc),
    );
    Ok(envelope)
}

fn map_message_delete(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        MessageDeleteEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidNotice {
            event_type: event_type.to_owned(),
            source,
        })?;
    validate_optional_rfc3339(event.message.timestamp.as_deref(), event_type)?;
    map_structured_event(adapter, payload, event_type, None, Event::Notice)
}

fn map_subscription_status(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event = SubscribeMessageStatusEvent::deserialize(&payload.d).map_err(|source| {
        MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        }
    })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidNotice {
            event_type: event_type.to_owned(),
            source,
        })?;
    let mut latest_update = None;
    for result in &event.result {
        unix_timestamp(result.subscribe_ts, event_type)?;
        unix_timestamp(result.update_ts, event_type)?;
        latest_update = Some(latest_update.map_or(result.update_ts, |current: u64| {
            current.max(result.update_ts)
        }));
    }
    let mut envelope = map_structured_event(adapter, payload, event_type, None, Event::Notice)?;
    envelope.timestamp = latest_update
        .map(|timestamp| unix_timestamp(timestamp, event_type))
        .transpose()?;
    Ok(envelope)
}

fn map_message_audit(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        MessageAuditEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    let outcome = if event_type == "MESSAGE_AUDIT_PASS" {
        MessageAuditOutcome::Pass
    } else {
        MessageAuditOutcome::Reject
    };
    event
        .validate(outcome)
        .map_err(|source| MappingError::InvalidNotice {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_structured_event(
        adapter,
        payload,
        event_type,
        Some("audit_time"),
        Event::Notice,
    )
}

fn map_guild_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event = GuildEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
        event_type: event_type.to_owned(),
        source,
    })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidGuildDispatch {
            event_type: event_type.to_owned(),
            source,
        })?;
    validate_optional_rfc3339(event.joined_at.as_deref(), event_type)?;
    map_structured_event(adapter, payload, event_type, None, Event::Notice)
}

fn map_channel_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event = ChannelEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
        event_type: event_type.to_owned(),
        source,
    })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidGuildDispatch {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_structured_event(adapter, payload, event_type, None, Event::Notice)
}

fn map_guild_member_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        GuildMemberEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidGuildDispatch {
            event_type: event_type.to_owned(),
            source,
        })?;
    validate_optional_rfc3339(Some(&event.joined_at), event_type)?;
    map_structured_event(adapter, payload, event_type, None, Event::Notice)
}

fn validate_optional_rfc3339(value: Option<&str>, event_type: &str) -> Result<(), MappingError> {
    if let Some(value) = value {
        DateTime::parse_from_rfc3339(value).map_err(|source| MappingError::InvalidTimestamp {
            event_type: event_type.to_owned(),
            source,
        })?;
    }
    Ok(())
}

fn unix_timestamp(timestamp: u64, event_type: &str) -> Result<DateTime<Utc>, MappingError> {
    i64::try_from(timestamp)
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .ok_or_else(|| MappingError::InvalidUnixTimestamp {
            event_type: event_type.to_owned(),
            timestamp,
        })
}

fn map_message_reaction(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let reaction =
        MessageReactionEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    reaction
        .validate()
        .map_err(|source| MappingError::InvalidReaction {
            event_type: event_type.to_owned(),
            source,
        })?;
    map_structured_event(
        adapter,
        payload,
        event_type,
        Some("timestamp"),
        Event::Notice,
    )
}

fn map_group_member(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let event =
        GroupMemberEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    event
        .validate()
        .map_err(|source| MappingError::InvalidGroupMember {
            event_type: event_type.to_owned(),
            source,
        })?;
    let timestamp = i64::try_from(event.timestamp)
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .ok_or_else(|| MappingError::InvalidUnixTimestamp {
            event_type: event_type.to_owned(),
            timestamp: event.timestamp,
        })?;
    let mut envelope = map_structured_event(adapter, payload, event_type, None, Event::Notice)?;
    envelope.timestamp = Some(timestamp);
    Ok(envelope)
}

fn map_interaction_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let interaction =
        InteractionEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    interaction
        .validate()
        .map_err(|source| MappingError::InvalidInteraction {
            event_type: event_type.to_owned(),
            source,
        })?;
    let wrap: fn(serde_json::Value) -> Event = if interaction.requires_response() {
        Event::Request
    } else {
        Event::Notice
    };
    map_structured_event_with_fallback_id(
        adapter,
        payload,
        event_type,
        Some("timestamp"),
        wrap,
        Some(&interaction.id),
    )
}

fn map_group_join_request(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
) -> Result<EventEnvelope, MappingError> {
    let request =
        GroupJoinRequestEvent::deserialize(&payload.d).map_err(|source| MappingError::Decode {
            event_type: event_type.to_owned(),
            source,
        })?;
    if request.group_openid.trim().is_empty()
        || request.request.join_request_id.trim().is_empty()
        || request.request.member_openid.trim().is_empty()
        || request.request.apply_source.trim().is_empty()
    {
        return Err(MappingError::InvalidEventData {
            event_type: event_type.to_owned(),
        });
    }
    map_structured_event(
        adapter,
        payload,
        event_type,
        Some("apply_at"),
        Event::Request,
    )
}

fn map_structured_event(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
    timestamp_field: Option<&str>,
    wrap: fn(serde_json::Value) -> Event,
) -> Result<EventEnvelope, MappingError> {
    map_structured_event_with_fallback_id(adapter, payload, event_type, timestamp_field, wrap, None)
}

fn map_structured_event_with_fallback_id(
    adapter: &AdapterId,
    payload: &GatewayPayload,
    event_type: &str,
    timestamp_field: Option<&str>,
    wrap: fn(serde_json::Value) -> Event,
    fallback_id: Option<&str>,
) -> Result<EventEnvelope, MappingError> {
    if !payload.d.is_object() {
        return Err(MappingError::InvalidEventData {
            event_type: event_type.to_owned(),
        });
    }
    let raw = serde_json::to_value(payload).unwrap_or_else(|_| payload.d.clone());
    let event_id = payload
        .id
        .clone()
        .filter(|value| !value.trim().is_empty())
        .map_or_else(
            || {
                if let Some(fallback_id) = fallback_id {
                    return Ok(fallback_id.to_owned());
                }
                let sequence = payload.s.ok_or_else(|| MappingError::MissingField {
                    event_type: event_type.to_owned(),
                    field: "dispatch id or sequence",
                })?;
                let digest = Sha256::digest(
                    serde_json::to_vec(&(event_type, &payload.d)).unwrap_or_default(),
                );
                let mut encoded = String::with_capacity(digest.len() * 2);
                for byte in digest {
                    let _ = write!(encoded, "{byte:02x}");
                }
                Ok(format!("qq:{event_type}:{sequence}:{encoded}"))
            },
            Ok,
        )?;
    let timestamp = match timestamp_field.and_then(|field| payload.d.get(field)) {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(value)) => Some(
            DateTime::parse_from_rfc3339(value)
                .map_err(|source| MappingError::InvalidTimestamp {
                    event_type: event_type.to_owned(),
                    source,
                })?
                .with_timezone(&Utc),
        ),
        Some(_) => {
            return Err(MappingError::InvalidEventData {
                event_type: event_type.to_owned(),
            });
        }
    };
    Ok(EventEnvelope {
        id: EventId::new(event_id),
        adapter: adapter.clone(),
        delivery_id: None,
        timestamp,
        event: wrap(serde_json::json!({
            "type": event_type,
            "data": payload.d,
        })),
        raw,
    })
}

fn required(
    value: Option<String>,
    event_type: &str,
    field: &'static str,
) -> Result<String, MappingError> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| MappingError::MissingField {
            event_type: event_type.to_owned(),
            field,
        })
}

#[cfg(test)]
mod tests {
    use bot_core::{AdapterId, Event, MessageTarget};
    use qqbot_protocol::{GatewayPayload, OpCode};
    use serde_json::json;

    use super::{MappingError, map_dispatch};

    #[test]
    fn maps_group_at_message() {
        let payload = GatewayPayload {
            id: Some("event-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "id":"message-id",
                "content":"/ping",
                "group_openid":"group-id",
                "author":{"member_openid":"member-id"}
            }),
            s: Some(2),
            t: Some("GROUP_AT_MESSAGE_CREATE".to_owned()),
        };

        let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        let Event::Message(message) = envelope.event else {
            panic!("expected common message");
        };
        assert_eq!(message.text, "/ping");
        assert_eq!(
            message.target,
            MessageTarget::Group {
                group_id: "group-id".to_owned()
            }
        );
    }

    #[test]
    fn maps_guild_direct_message_to_private_scoped_target() {
        let payload = GatewayPayload {
            id: Some("event-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "id":"message-id",
                "content":"/ping",
                "guild_id":"direct-guild-id",
                "channel_id":"direct-channel-id",
                "author":{"id":"member-id"}
            }),
            s: Some(2),
            t: Some("DIRECT_MESSAGE_CREATE".to_owned()),
        };

        let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        let Event::Message(message) = envelope.event else {
            panic!("expected common message");
        };
        assert_eq!(
            message.target,
            MessageTarget::GuildDirect {
                guild_id: "direct-guild-id".to_owned()
            }
        );
        assert_eq!(message.scope(), bot_core::MessageScope::Private);
    }

    #[test]
    fn maps_all_social_status_notices_with_unix_timestamps() {
        let timestamp = 1_784_570_617_u64;
        let cases = [
            (
                "FRIEND_ADD",
                json!({
                    "openid":"user-id","timestamp":timestamp,"scene":9001,
                    "scene_param":"callback-data","author":{
                        "union_openid":"union-id","future_nested":true
                    },"future":true
                }),
            ),
            (
                "FRIEND_DEL",
                json!({"openid":"user-id","timestamp":timestamp,"future":true}),
            ),
            (
                "GROUP_ADD_ROBOT",
                json!({
                    "group_openid":"group-id","op_member_openid":"member-id",
                    "timestamp":timestamp,"future":true
                }),
            ),
            (
                "GROUP_DEL_ROBOT",
                json!({
                    "group_openid":"group-id","op_member_openid":"member-id",
                    "timestamp":timestamp,"future":true
                }),
            ),
            (
                "GROUP_MSG_RECEIVE",
                json!({
                    "group_openid":"group-id","op_member_openid":"member-id",
                    "timestamp":timestamp,"future":true
                }),
            ),
            (
                "GROUP_MSG_REJECT",
                json!({
                    "group_openid":"group-id","op_member_openid":"member-id",
                    "timestamp":timestamp,"future":true
                }),
            ),
            (
                "C2C_MSG_RECEIVE",
                json!({"openid":"user-id","timestamp":timestamp,"future":true}),
            ),
            (
                "C2C_MSG_REJECT",
                json!({"openid":"user-id","timestamp":timestamp,"future":true}),
            ),
        ];

        for (sequence, (event_type, data)) in cases.into_iter().enumerate() {
            let payload = GatewayPayload {
                id: Some(format!("notice-{sequence}")),
                op: OpCode::DISPATCH,
                d: data,
                s: Some(sequence as u64),
                t: Some(event_type.to_owned()),
            };
            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            assert_eq!(envelope.id.as_str(), format!("notice-{sequence}"));
            assert_eq!(
                envelope.timestamp.unwrap().timestamp(),
                i64::try_from(timestamp).unwrap()
            );
            assert_eq!(envelope.raw["d"]["future"], true);
            let Event::Notice(notice) = envelope.event else {
                panic!("expected notice event");
            };
            assert_eq!(notice["type"], event_type);
            assert_eq!(notice["data"], payload.d);
        }
    }

    #[test]
    fn rejects_invalid_social_status_notices() {
        let base = GatewayPayload {
            id: Some("notice-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({"openid":"user-id","timestamp":"2026-08-22T10:00:00Z"}),
            s: Some(3),
            t: Some("C2C_MSG_RECEIVE".to_owned()),
        };
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &base),
            Err(MappingError::Decode { .. })
        ));

        let mut overflow = base.clone();
        overflow.d = json!({"openid":"user-id","timestamp":u64::MAX});
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &overflow),
            Err(MappingError::InvalidNotice { .. })
        ));

        let mut invalid_identifier = base;
        invalid_identifier.d = json!({"openid":"user id","timestamp":1_784_570_617_u64});
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &invalid_identifier),
            Err(MappingError::InvalidNotice { .. })
        ));
    }

    #[test]
    fn validates_and_maps_guild_lifecycle_events() {
        let events = [
            (
                "GUILD_CREATE",
                json!({
                    "id":"guild-id",
                    "name":"guild",
                    "icon":"https://example.com/icon.png",
                    "owner_id":"owner-id",
                    "member_count":100,
                    "max_members":1000,
                    "description":"description",
                    "joined_at":"2026-01-01T00:00:00+08:00",
                    "op_user_id":"operator-id",
                    "__none":1
                }),
            ),
            (
                "GUILD_UPDATE",
                json!({
                    "id":"guild-id",
                    "name":"updated guild",
                    "icon":"https://example.com/icon.png",
                    "owner_id":"owner-id",
                    "member_count":12,
                    "max_members":1000,
                    "description":"updated description"
                }),
            ),
            (
                "GUILD_DELETE",
                json!({
                    "id":"guild-id",
                    "name":"deleted guild",
                    "icon":"https://example.com/icon.png",
                    "owner_id":"owner-id",
                    "member_count":12,
                    "max_members":1000,
                    "description":"deleted"
                }),
            ),
        ];
        for (event_type, data) in events {
            let payload = GatewayPayload {
                id: Some(format!("{event_type}-id")),
                op: OpCode::DISPATCH,
                d: data,
                s: Some(4),
                t: Some(event_type.to_owned()),
            };
            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            assert!(envelope.timestamp.is_none());
            let Event::Notice(notice) = envelope.event else {
                panic!("expected guild notice");
            };
            assert_eq!(notice["type"], event_type);
            assert_eq!(notice["data"]["id"], "guild-id");
            if event_type == "GUILD_CREATE" {
                assert_eq!(notice["data"]["__none"], 1);
            }
        }
    }

    #[test]
    fn validates_and_maps_channel_lifecycle_events() {
        for event_type in ["CHANNEL_CREATE", "CHANNEL_UPDATE", "CHANNEL_DELETE"] {
            let payload = GatewayPayload {
                id: Some(format!("{event_type}-id")),
                op: OpCode::DISPATCH,
                d: json!({
                    "id":"channel-id",
                    "guild_id":"guild-id",
                    "name":"channel",
                    "type":0,
                    "sub_type":0,
                    "position":1,
                    "owner_id":"owner-id",
                    "__none":1
                }),
                s: Some(4),
                t: Some(event_type.to_owned()),
            };
            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            assert!(envelope.timestamp.is_none());
            let Event::Notice(notice) = envelope.event else {
                panic!("expected channel notice");
            };
            assert_eq!(notice["type"], event_type);
            assert_eq!(notice["data"]["position"], 1);
            assert_eq!(notice["data"]["__none"], 1);
        }
    }

    #[test]
    fn validates_and_maps_guild_member_events() {
        for event_type in [
            "GUILD_MEMBER_ADD",
            "GUILD_MEMBER_UPDATE",
            "GUILD_MEMBER_REMOVE",
        ] {
            let payload = GatewayPayload {
                id: Some(format!("{event_type}-id")),
                op: OpCode::DISPATCH,
                d: json!({
                    "guild_id":"guild-id",
                    "joined_at":"2021-10-21T11:20:18+08:00",
                    "nick":"",
                    "op_user_id":"operator-id",
                    "roles":[],
                    "user":{
                        "id":"user-id",
                        "username":"member",
                        "avatar":"https://example.com/avatar.png",
                        "bot":false
                    },
                    "mute":false,
                    "__none":1
                }),
                s: Some(4),
                t: Some(event_type.to_owned()),
            };
            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            assert!(envelope.timestamp.is_none());
            let Event::Notice(notice) = envelope.event else {
                panic!("expected guild member notice");
            };
            assert_eq!(notice["type"], event_type);
            assert_eq!(notice["data"]["nick"], "");
            assert_eq!(notice["data"]["roles"], json!([]));
            assert_eq!(notice["data"]["mute"], false);
            assert_eq!(notice["data"]["__none"], 1);
        }
    }

    #[test]
    fn rejects_malformed_guild_channel_and_member_events() {
        let guild = GatewayPayload {
            id: Some("guild-event-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "id":"guild-id",
                "name":"guild",
                "icon":"https://example.com/icon.png",
                "owner_id":"owner-id",
                "member_count":12,
                "max_members":1000,
                "description":"description",
                "joined_at":"not-a-timestamp"
            }),
            s: Some(4),
            t: Some("GUILD_CREATE".to_owned()),
        };
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &guild),
            Err(MappingError::InvalidTimestamp { .. })
        ));
        let mut blank_guild = guild;
        blank_guild.d["joined_at"] = json!("2026-01-01T00:00:00+08:00");
        blank_guild.d["id"] = json!(" ");
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &blank_guild),
            Err(MappingError::InvalidGuildDispatch { source, .. }) if source.field == "id"
        ));

        let channel = GatewayPayload {
            id: Some("channel-event-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "id":"channel-id",
                "guild_id":" ",
                "name":"channel",
                "type":0,
                "sub_type":0,
                "owner_id":"owner-id"
            }),
            s: Some(4),
            t: Some("CHANNEL_UPDATE".to_owned()),
        };
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &channel),
            Err(MappingError::InvalidGuildDispatch { source, .. })
                if source.field == "guild_id"
        ));

        let member = GatewayPayload {
            id: Some("member-event-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "guild_id":"guild-id",
                "joined_at":"not-a-timestamp",
                "nick":"member",
                "op_user_id":"operator-id",
                "roles":["2"],
                "user":{
                    "id":"user-id",
                    "username":"member",
                    "avatar":"https://example.com/avatar.png",
                    "bot":false
                }
            }),
            s: Some(4),
            t: Some("GUILD_MEMBER_ADD".to_owned()),
        };
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &member),
            Err(MappingError::InvalidTimestamp { .. })
        ));
        let mut missing_joined_at = member.clone();
        missing_joined_at
            .d
            .as_object_mut()
            .unwrap()
            .remove("joined_at");
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &missing_joined_at),
            Err(MappingError::Decode { .. })
        ));
        let mut missing_operator = member;
        missing_operator
            .d
            .as_object_mut()
            .unwrap()
            .remove("op_user_id");
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &missing_operator),
            Err(MappingError::Decode { .. })
        ));
    }

    #[test]
    fn validates_and_maps_message_reaction_events() {
        for event_type in ["MESSAGE_REACTION_ADD", "MESSAGE_REACTION_REMOVE"] {
            let payload = GatewayPayload {
                id: Some(format!("{event_type}-id")),
                op: OpCode::DISPATCH,
                d: json!({
                    "user_id":"user-id",
                    "emoji":{"id":"203","type":1},
                    "channel_id":"channel-id",
                    "guild_id":"guild-id",
                    "target":{"id":"message-id","type":0}
                }),
                s: Some(4),
                t: Some(event_type.to_owned()),
            };
            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            let Event::Notice(notice) = envelope.event else {
                panic!("expected reaction notice");
            };
            assert_eq!(notice["type"], event_type);
            assert_eq!(notice["data"]["emoji"]["id"], "203");
            assert_eq!(notice["data"]["target"]["id"], "message-id");
        }
    }

    #[test]
    fn rejects_malformed_message_reaction_events() {
        let base = GatewayPayload {
            id: Some("reaction-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "user_id":"user-id",
                "emoji":{"id":"203","type":1},
                "channel_id":"channel-id",
                "guild_id":"guild-id",
                "target":{"id":"message-id","type":0}
            }),
            s: Some(4),
            t: Some("MESSAGE_REACTION_ADD".to_owned()),
        };
        for invalid in [
            json!({
                "emoji":{"id":"203","type":1},
                "channel_id":"channel-id",
                "guild_id":"guild-id",
                "target":{"id":"message-id","type":0}
            }),
            json!({
                "user_id":"user-id",
                "emoji":{"id":"203","type":3},
                "channel_id":"channel-id",
                "guild_id":"guild-id",
                "target":{"id":"message-id","type":0}
            }),
            json!({
                "user_id":"user-id",
                "emoji":{"id":"203","type":1},
                "channel_id":"channel-id",
                "guild_id":"guild-id",
                "target":{"id":" ","type":0}
            }),
            json!({
                "user_id":"user-id",
                "emoji":{"id":"203","type":1},
                "channel_id":"channel-id",
                "guild_id":"guild-id",
                "target":{"id":"message-id","type":4}
            }),
        ] {
            let payload = GatewayPayload {
                d: invalid,
                ..base.clone()
            };
            assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
        }
    }

    #[test]
    fn validates_and_maps_group_member_events() {
        for event_type in ["GROUP_MEMBER_ADD", "GROUP_MEMBER_REMOVE"] {
            let payload = GatewayPayload {
                id: Some(format!("{event_type}-id")),
                op: OpCode::DISPATCH,
                d: json!({
                    "timestamp":1_787_392_800,
                    "group_openid":"group-id",
                    "member_openid":"member-id",
                    "user_openid":"",
                    "__none":1
                }),
                s: Some(5),
                t: Some(event_type.to_owned()),
            };
            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            assert_eq!(
                envelope.timestamp.unwrap().to_rfc3339(),
                "2026-08-22T10:00:00+00:00"
            );
            let Event::Notice(notice) = envelope.event else {
                panic!("expected group member notice");
            };
            assert_eq!(notice["type"], event_type);
            assert_eq!(
                notice["data"],
                json!({
                    "timestamp":1_787_392_800,
                    "group_openid":"group-id",
                    "member_openid":"member-id",
                    "user_openid":"",
                    "__none":1
                })
            );
        }
    }

    #[test]
    fn rejects_malformed_group_member_events() {
        let base = GatewayPayload {
            id: Some("group-member-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "timestamp":1_787_392_800,
                "group_openid":"group-id",
                "member_openid":"member-id",
                "user_openid":"user-id"
            }),
            s: Some(5),
            t: Some("GROUP_MEMBER_ADD".to_owned()),
        };
        let missing_user = GatewayPayload {
            d: json!({
                "timestamp":1_787_392_800,
                "group_openid":"group-id",
                "member_openid":"member-id"
            }),
            ..base.clone()
        };
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &missing_user),
            Err(MappingError::Decode { .. })
        ));

        let blank_group = GatewayPayload {
            d: json!({
                "timestamp":1_787_392_800,
                "group_openid":" ",
                "member_openid":"member-id",
                "user_openid":"user-id"
            }),
            ..base.clone()
        };
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &blank_group),
            Err(MappingError::InvalidGroupMember { source, .. })
                if source.field == "group_openid"
        ));

        let invalid_timestamp = GatewayPayload {
            d: json!({
                "timestamp":18_446_744_073_709_551_615_u64,
                "group_openid":"group-id",
                "member_openid":"member-id",
                "user_openid":"user-id"
            }),
            ..base
        };
        assert!(matches!(
            map_dispatch(&AdapterId::new("qq"), &invalid_timestamp),
            Err(MappingError::InvalidUnixTimestamp { timestamp, .. })
                if timestamp == u64::MAX
        ));
    }

    #[test]
    fn maps_interaction_requests_and_notices_with_data_id() {
        for (interaction_type, expected_request) in [
            (11, true),
            (12, true),
            (13, false),
            (18, false),
            (999, false),
        ] {
            let payload = GatewayPayload {
                id: Some(format!("gateway-{interaction_type}")),
                op: OpCode::DISPATCH,
                d: json!({
                    "id":format!("interaction-{interaction_type}"),
                    "type":interaction_type,
                    "scene":"group",
                    "chat_type":1,
                    "timestamp":"2026-08-22T10:00:00+08:00",
                    "group_openid":"group-id",
                    "group_member_openid":"member-id",
                    "data":{
                        "type":interaction_type,
                        "resolved":{"button_data":"approve"},
                        "future_data":true
                    },
                    "future_top":"kept"
                }),
                s: Some(8),
                t: Some("INTERACTION_CREATE".to_owned()),
            };

            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            assert_eq!(envelope.id.as_str(), format!("gateway-{interaction_type}"));
            assert_eq!(
                envelope.timestamp.unwrap().to_rfc3339(),
                "2026-08-22T02:00:00+00:00"
            );
            let body = match envelope.event {
                Event::Request(body) if expected_request => body,
                Event::Notice(body) if !expected_request => body,
                other => panic!("unexpected interaction event: {other:?}"),
            };
            assert_eq!(body["type"], "INTERACTION_CREATE");
            assert_eq!(
                body["data"]["id"],
                format!("interaction-{interaction_type}")
            );
            assert_eq!(body["data"]["future_top"], "kept");
            assert_eq!(body["data"]["data"]["future_data"], true);
        }
    }

    #[test]
    fn maps_c2c_and_guild_interaction_scenes() {
        for (data, expected_request, id_field, id_value) in [
            (
                json!({
                    "id":"c2c-interaction","type":12,"scene":"c2c",
                    "timestamp":"2026-08-22T10:00:00Z","user_openid":"user-id"
                }),
                true,
                "user_openid",
                "user-id",
            ),
            (
                json!({
                    "id":"guild-interaction","type":13,"scene":"guild",
                    "timestamp":"2026-08-22T10:00:00Z","guild_id":"guild-id",
                    "channel_id":"channel-id"
                }),
                false,
                "channel_id",
                "channel-id",
            ),
        ] {
            let payload = GatewayPayload {
                id: Some("gateway-interaction".to_owned()),
                op: OpCode::DISPATCH,
                d: data,
                s: Some(9),
                t: Some("INTERACTION_CREATE".to_owned()),
            };
            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            let body = match envelope.event {
                Event::Request(body) if expected_request => body,
                Event::Notice(body) if !expected_request => body,
                other => panic!("unexpected interaction event: {other:?}"),
            };
            assert_eq!(body["data"][id_field], id_value);
        }

        let payload = GatewayPayload {
            id: None,
            op: OpCode::DISPATCH,
            d: json!({
                "id":"interaction-fallback","type":12,"scene":"c2c",
                "timestamp":"2026-08-22T10:00:00Z","user_openid":"user-id"
            }),
            s: None,
            t: Some("INTERACTION_CREATE".to_owned()),
        };
        let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        assert_eq!(envelope.id.as_str(), "interaction-fallback");

        let sparse_payload = GatewayPayload {
            id: Some("sparse-interaction".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "id":"sparse-interaction","type":13,"scene":"group",
                "timestamp":"2026-08-22T10:00:00Z"
            }),
            s: Some(10),
            t: Some("INTERACTION_CREATE".to_owned()),
        };
        assert!(
            map_dispatch(&AdapterId::new("qq"), &sparse_payload)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn rejects_malformed_interaction_events() {
        let base = GatewayPayload {
            id: Some("gateway-interaction-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "id":"interaction-id",
                "type":11,
                "timestamp":"2026-08-22T10:00:00Z"
            }),
            s: Some(8),
            t: Some("INTERACTION_CREATE".to_owned()),
        };
        for (data, expected_decode) in [
            (json!({"type":11,"timestamp":"2026-08-22T10:00:00Z"}), true),
            (
                json!({"id":"interaction-id","timestamp":"2026-08-22T10:00:00Z"}),
                true,
            ),
            (
                json!({"id":" ","type":11,"timestamp":"2026-08-22T10:00:00Z"}),
                false,
            ),
            (
                json!({
                    "id":"interaction-id","type":11,
                    "timestamp":"2026-08-22T10:00:00Z","group_openid":" "
                }),
                false,
            ),
            (
                json!({
                    "id":"interaction-id","type":11,
                    "timestamp":"2026-08-22T10:00:00Z","user_openid":"bad id"
                }),
                false,
            ),
            (
                json!({
                    "id":"interaction-id","type":13,
                    "timestamp":"2026-08-22T10:00:00Z","data":{"type":11}
                }),
                false,
            ),
            (
                json!({"id":"interaction-id","type":11,"timestamp":"not-a-timestamp"}),
                false,
            ),
            (json!([]), true),
        ] {
            let payload = GatewayPayload {
                d: data,
                ..base.clone()
            };
            let error = map_dispatch(&AdapterId::new("qq"), &payload).unwrap_err();
            if expected_decode {
                assert!(matches!(error, MappingError::Decode { .. }));
            } else {
                assert!(matches!(error, MappingError::InvalidInteraction { .. }));
            }
        }
    }

    #[test]
    fn maps_group_join_request_as_request_event() {
        let payload = GatewayPayload {
            id: Some("request-event-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "group_openid":"group-id",
                "join_request_id":"join-request-id",
                "member_openid":"member-id",
                "apply_at":"2099-08-10T10:00:00Z",
                "apply_source":"self_apply"
            }),
            s: Some(4),
            t: Some("GROUP_JOIN_REQUEST".to_owned()),
        };

        let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        assert_eq!(
            envelope.timestamp.unwrap().to_rfc3339(),
            "2099-08-10T10:00:00+00:00"
        );
        let Event::Request(request) = envelope.event else {
            panic!("expected request event");
        };
        assert_eq!(request["type"], "GROUP_JOIN_REQUEST");
        assert_eq!(request["data"]["join_request_id"], "join-request-id");
    }

    #[test]
    fn rejects_malformed_group_join_requests() {
        let base = GatewayPayload {
            id: Some("request-event-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "group_openid":"group-id",
                "join_request_id":"join-request-id",
                "member_openid":"member-id",
                "apply_at":"2099-08-10T10:00:00Z",
                "apply_source":"self_apply"
            }),
            s: Some(4),
            t: Some("GROUP_JOIN_REQUEST".to_owned()),
        };

        for invalid in [
            json!({
                "join_request_id":"join-request-id",
                "member_openid":"member-id",
                "apply_at":"2099-08-10T10:00:00Z",
                "apply_source":"self_apply"
            }),
            json!({
                "group_openid":" ",
                "join_request_id":"join-request-id",
                "member_openid":"member-id",
                "apply_at":"2099-08-10T10:00:00Z",
                "apply_source":"self_apply"
            }),
            json!({
                "group_openid":"group-id",
                "join_request_id":" ",
                "member_openid":"member-id",
                "apply_at":"2099-08-10T10:00:00Z",
                "apply_source":"self_apply"
            }),
            json!({
                "group_openid":"group-id",
                "join_request_id":"join-request-id",
                "member_openid":" ",
                "apply_at":"2099-08-10T10:00:00Z",
                "apply_source":"self_apply"
            }),
            json!({
                "group_openid":"group-id",
                "join_request_id":"join-request-id",
                "member_openid":"member-id",
                "apply_at":"2099-08-10T10:00:00Z",
                "apply_source":" "
            }),
            json!({
                "group_openid":"group-id",
                "join_request_id":"join-request-id",
                "member_openid":"member-id",
                "apply_at":42,
                "apply_source":"self_apply"
            }),
            json!({
                "group_openid":"group-id",
                "join_request_id":"join-request-id",
                "member_openid":"member-id",
                "apply_at":"not-a-timestamp",
                "apply_source":"self_apply"
            }),
        ] {
            let mut payload = base.clone();
            payload.d = invalid;
            assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
        }
    }

    #[test]
    fn maps_all_channel_message_delete_notices() {
        for event_type in [
            "MESSAGE_DELETE",
            "PUBLIC_MESSAGE_DELETE",
            "DIRECT_MESSAGE_DELETE",
        ] {
            let payload = GatewayPayload {
                id: Some(format!("{event_type}-id")),
                op: OpCode::DISPATCH,
                d: json!({
                    "message": {
                        "id":"message-id",
                        "guild_id":"guild-id",
                        "channel_id":"channel-id",
                        "timestamp":"2026-08-22T10:00:00+08:00",
                        "author":{"id":"author-id"},
                        "future":true
                    },
                    "op_user":{"id":"operator-id","future":true},
                    "future":true
                }),
                s: Some(20),
                t: Some(event_type.to_owned()),
            };

            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            assert_eq!(envelope.timestamp, None);
            let Event::Notice(body) = envelope.event else {
                panic!("expected notice");
            };
            assert_eq!(body["type"], event_type);
            assert_eq!(body["data"]["message"]["id"], "message-id");
            assert_eq!(body["data"]["op_user"]["id"], "operator-id");
            assert_eq!(envelope.raw["d"]["future"], true);
        }
    }

    #[test]
    fn rejects_malformed_channel_message_delete_notices() {
        let base = GatewayPayload {
            id: Some("delete-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "message": {
                    "id":"message-id",
                    "guild_id":"guild-id",
                    "channel_id":"channel-id",
                    "timestamp":"2026-08-22T10:00:00+08:00"
                },
                "op_user":{"id":"operator-id"}
            }),
            s: Some(20),
            t: Some("PUBLIC_MESSAGE_DELETE".to_owned()),
        };
        for invalid in [
            json!({
                "message":{"id":" ","guild_id":"guild-id","channel_id":"channel-id"},
                "op_user":{"id":"operator-id"}
            }),
            json!({
                "message":{"id":"message-id","guild_id":" ","channel_id":"channel-id"},
                "op_user":{"id":"operator-id"}
            }),
            json!({
                "message":{"id":"message-id","guild_id":"guild-id","channel_id":" "},
                "op_user":{"id":"operator-id"}
            }),
            json!({
                "message":{"id":"message-id","guild_id":"guild-id","channel_id":"channel-id"},
                "op_user":{"id":" "}
            }),
            json!({
                "message":{
                    "id":"message-id","guild_id":"guild-id","channel_id":"channel-id",
                    "timestamp":"not-a-timestamp"
                },
                "op_user":{"id":"operator-id"}
            }),
        ] {
            let mut payload = base.clone();
            payload.d = invalid;
            assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
        }
    }

    #[test]
    fn maps_subscription_status_targets_and_latest_update_time() {
        for target in [
            json!({"openid":"user-openid"}),
            json!({"group_openid":"group-openid"}),
            json!({"openid":"user-openid","group_openid":"group-openid"}),
        ] {
            let mut data = target;
            data["result"] = json!([
                {
                    "template_id":10001,"custom_template_id":"custom-1","op":1,
                    "subscribe_id":"subscribe-1","subscribe_ts":1_784_276_800,
                    "update_ts":1_784_276_810
                },
                {
                    "template_id":10002,"custom_template_id":"custom-2","op":99,
                    "subscribe_id":"subscribe-2","subscribe_ts":1_784_276_805,
                    "update_ts":1_784_276_820,"future":true
                },
                {
                    "template_id":10003,"custom_template_id":"custom-3","op":2,
                    "subscribe_id":"subscribe-3","subscribe_ts":1_784_276_806,
                    "update_ts":1_784_276_815
                }
            ]);
            let payload = GatewayPayload {
                id: Some("subscription-id".to_owned()),
                op: OpCode::DISPATCH,
                d: data,
                s: Some(21),
                t: Some("SUBSCRIBE_MESSAGE_STATUS".to_owned()),
            };
            let envelope = map_dispatch(&AdapterId::new("qq"), &payload)
                .unwrap()
                .unwrap();
            assert_eq!(
                envelope.timestamp.map(|timestamp| timestamp.timestamp()),
                Some(1_784_276_820)
            );
            let Event::Notice(body) = envelope.event else {
                panic!("expected notice");
            };
            assert_eq!(body["data"]["result"][1]["op"], 99);
            assert_eq!(body["data"]["result"][2]["op"], 2);
        }

        let empty = GatewayPayload {
            id: Some("subscription-empty-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({"openid":"user-openid","result":[]}),
            s: Some(22),
            t: Some("SUBSCRIBE_MESSAGE_STATUS".to_owned()),
        };
        assert_eq!(
            map_dispatch(&AdapterId::new("qq"), &empty)
                .unwrap()
                .unwrap()
                .timestamp,
            None
        );
    }

    #[test]
    fn rejects_invalid_subscription_status_notices() {
        for data in [
            json!({"result":[]}),
            json!({
                "openid":"user-openid",
                "result":[{
                    "template_id":1,"custom_template_id":"custom","op":1,
                    "subscribe_id":"subscribe","subscribe_ts":u64::MAX,"update_ts":1
                }]
            }),
            json!({
                "openid":"user-openid",
                "result":[{
                    "template_id":1,"custom_template_id":"custom","op":1,
                    "subscribe_id":"subscribe","subscribe_ts":1,"update_ts":u64::MAX
                }]
            }),
        ] {
            let payload = GatewayPayload {
                id: Some("subscription-id".to_owned()),
                op: OpCode::DISPATCH,
                d: data,
                s: Some(23),
                t: Some("SUBSCRIBE_MESSAGE_STATUS".to_owned()),
            };
            assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
        }
    }

    #[test]
    fn maps_message_audit_outcome_from_dispatch_type() {
        let base = GatewayPayload {
            id: Some("audit-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({
                "audit_id":"audit-record-id",
                "message_id":"message-id",
                "guild_id":"guild-id",
                "channel_id":"channel-id",
                "audit_time":"2026-08-22T10:01:00+08:00",
                "create_time":"2026-08-22T10:00:00+08:00",
                "future":true
            }),
            s: Some(24),
            t: Some("MESSAGE_AUDIT_PASS".to_owned()),
        };
        let passed = map_dispatch(&AdapterId::new("qq"), &base).unwrap().unwrap();
        assert_eq!(
            passed.timestamp.unwrap().to_rfc3339(),
            "2026-08-22T02:01:00+00:00"
        );

        for message_id in [serde_json::Value::Null, json!("")] {
            let mut rejected = base.clone();
            rejected.t = Some("MESSAGE_AUDIT_REJECT".to_owned());
            rejected.d["message_id"] = message_id;
            let envelope = map_dispatch(&AdapterId::new("qq"), &rejected)
                .unwrap()
                .unwrap();
            let Event::Notice(body) = envelope.event else {
                panic!("expected notice");
            };
            assert_eq!(body["type"], "MESSAGE_AUDIT_REJECT");
        }

        let mut rejected_without_message_id = base.clone();
        rejected_without_message_id.t = Some("MESSAGE_AUDIT_REJECT".to_owned());
        rejected_without_message_id
            .d
            .as_object_mut()
            .unwrap()
            .remove("message_id");
        assert!(
            map_dispatch(&AdapterId::new("qq"), &rejected_without_message_id)
                .unwrap()
                .is_some()
        );

        let mut rejected_with_message_id = base.clone();
        rejected_with_message_id.t = Some("MESSAGE_AUDIT_REJECT".to_owned());
        assert!(
            map_dispatch(&AdapterId::new("qq"), &rejected_with_message_id)
                .unwrap()
                .is_some()
        );

        let mut missing_pass_id = base;
        missing_pass_id.d["message_id"] = serde_json::Value::Null;
        assert!(map_dispatch(&AdapterId::new("qq"), &missing_pass_id).is_err());
    }

    #[test]
    fn rejects_invalid_message_audit_timestamps_and_ignores_invented_delete_events() {
        for (field, value) in [
            ("audit_time", json!("not-a-timestamp")),
            ("create_time", json!("not-a-timestamp")),
            ("audit_time", json!(42)),
        ] {
            let mut data = json!({
                "audit_id":"audit-record-id",
                "message_id":"message-id",
                "guild_id":"guild-id",
                "channel_id":"channel-id",
                "audit_time":"2026-08-22T10:01:00+08:00",
                "create_time":"2026-08-22T10:00:00+08:00"
            });
            data[field] = value;
            let payload = GatewayPayload {
                id: Some("audit-id".to_owned()),
                op: OpCode::DISPATCH,
                d: data,
                s: Some(25),
                t: Some("MESSAGE_AUDIT_PASS".to_owned()),
            };
            assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
        }

        for event_type in [
            "C2C_MESSAGE_DELETE",
            "GROUP_MESSAGE_DELETE",
            "GROUP_AT_MESSAGE_DELETE",
        ] {
            let payload = GatewayPayload {
                id: Some("invented-id".to_owned()),
                op: OpCode::DISPATCH,
                d: json!({}),
                s: Some(26),
                t: Some(event_type.to_owned()),
            };
            assert!(
                map_dispatch(&AdapterId::new("qq"), &payload)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn derives_stable_notice_id_when_dispatch_id_is_absent() {
        let payload = GatewayPayload {
            id: None,
            op: OpCode::DISPATCH,
            d: json!({
                "group_openid":"group-id",
                "op_member_openid":"member-id",
                "timestamp":1_784_570_617_u64
            }),
            s: Some(4),
            t: Some("GROUP_MSG_RECEIVE".to_owned()),
        };

        let first = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        let second = map_dispatch(&AdapterId::new("qq"), &payload)
            .unwrap()
            .unwrap();
        assert_eq!(first.id, second.id);
        assert!(first.id.as_str().starts_with("qq:GROUP_MSG_RECEIVE:4:"));

        let mut next_delivery = payload;
        next_delivery.s = Some(5);
        let third = map_dispatch(&AdapterId::new("qq"), &next_delivery)
            .unwrap()
            .unwrap();
        assert_ne!(first.id, third.id);
    }

    #[test]
    fn rejects_malformed_notice_timestamp() {
        let mut payload = GatewayPayload {
            id: Some("notice-id".to_owned()),
            op: OpCode::DISPATCH,
            d: json!({"timestamp":"not-a-timestamp"}),
            s: Some(6),
            t: Some("GUILD_UPDATE".to_owned()),
        };

        assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
        payload.d = json!({"timestamp":42});
        assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
        payload.d = json!([]);
        assert!(map_dispatch(&AdapterId::new("qq"), &payload).is_err());
    }
}
