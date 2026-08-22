//! Gateway payload, opcode, intent, and endpoint types.

use std::fmt;

use bitflags::bitflags;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// A QQ Gateway opcode.
///
/// Unknown values are preserved so protocol additions do not break decoding.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpCode(u8);

impl OpCode {
    pub const DISPATCH: Self = Self(0);
    pub const HEARTBEAT: Self = Self(1);
    pub const IDENTIFY: Self = Self(2);
    pub const RESUME: Self = Self(6);
    pub const RECONNECT: Self = Self(7);
    pub const INVALID_SESSION: Self = Self(9);
    pub const HELLO: Self = Self(10);
    pub const HEARTBEAT_ACK: Self = Self(11);
    pub const HTTP_CALLBACK_ACK: Self = Self(12);
    pub const CALLBACK_VALIDATION: Self = Self(13);

    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    pub const fn value(self) -> u8 {
        self.0
    }
}

impl fmt::Debug for OpCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            Self::DISPATCH => "Dispatch",
            Self::HEARTBEAT => "Heartbeat",
            Self::IDENTIFY => "Identify",
            Self::RESUME => "Resume",
            Self::RECONNECT => "Reconnect",
            Self::INVALID_SESSION => "InvalidSession",
            Self::HELLO => "Hello",
            Self::HEARTBEAT_ACK => "HeartbeatAck",
            Self::HTTP_CALLBACK_ACK => "HttpCallbackAck",
            Self::CALLBACK_VALIDATION => "CallbackValidation",
            _ => "Unknown",
        };
        formatter
            .debug_struct("OpCode")
            .field("name", &name)
            .field("value", &self.0)
            .finish()
    }
}

impl Serialize for OpCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(self.0)
    }
}

impl<'de> Deserialize<'de> for OpCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        u8::deserialize(deserializer).map(Self)
    }
}

bitflags! {
    /// Event subscriptions requested during Gateway Identify.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Intents: u32 {
        const GUILDS = 1 << 0;
        const GUILD_MEMBERS = 1 << 1;
        const GUILD_MESSAGES = 1 << 9;
        const GUILD_MESSAGE_REACTIONS = 1 << 10;
        const DIRECT_MESSAGE = 1 << 12;
        const OPEN_FORUMS_EVENT = 1 << 18;
        const AUDIO_OR_LIVE_CHANNEL_MEMBER = 1 << 19;
        const GROUP_MEMBER_EVENT = 1 << 24;
        const GROUP_AND_C2C_EVENT = 1 << 25;
        const INTERACTION = 1 << 26;
        const MESSAGE_AUDIT = 1 << 27;
        const FORUMS_EVENT = 1 << 28;
        const AUDIO_ACTION = 1 << 29;
        const PUBLIC_GUILD_MESSAGES = 1 << 30;
    }
}

impl Intents {
    #[must_use]
    pub const fn with_guild_messages(self) -> Self {
        self.union(Self::GUILD_MESSAGES)
    }

    #[must_use]
    pub const fn with_group_and_c2c(self) -> Self {
        self.union(Self::GROUP_AND_C2C_EVENT)
    }

    #[must_use]
    pub const fn with_public_guild_messages(self) -> Self {
        self.union(Self::PUBLIC_GUILD_MESSAGES)
    }

    #[must_use]
    pub const fn with_direct_messages(self) -> Self {
        self.union(Self::DIRECT_MESSAGE)
    }

    #[must_use]
    pub const fn with_interactions(self) -> Self {
        self.union(Self::INTERACTION)
    }

    #[must_use]
    pub const fn with_open_forums(self) -> Self {
        self.union(Self::OPEN_FORUMS_EVENT)
    }

    #[must_use]
    pub const fn with_audio_live_members(self) -> Self {
        self.union(Self::AUDIO_OR_LIVE_CHANNEL_MEMBER)
    }

    #[must_use]
    pub const fn with_forums(self) -> Self {
        self.union(Self::FORUMS_EVENT)
    }

    #[must_use]
    pub const fn with_audio_actions(self) -> Self {
        self.union(Self::AUDIO_ACTION)
    }
}

impl Serialize for Intents {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u32(self.bits())
    }
}

impl<'de> Deserialize<'de> for Intents {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        u32::deserialize(deserializer).map(Self::from_bits_retain)
    }
}

/// The common envelope used by all Gateway payloads.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatewayPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub op: OpCode,
    #[serde(default)]
    pub d: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub t: Option<String>,
}

/// Response from the unsharded `/gateway` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Gateway {
    pub url: String,
}

/// Response from the `/gateway/bot` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GatewayBot {
    pub url: String,
    pub shards: u32,
    pub session_start_limit: SessionStartLimit,
}

/// Gateway session creation limits returned by QQ.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionStartLimit {
    pub total: u32,
    pub remaining: u32,
    pub reset_after: u64,
    pub max_concurrency: u32,
}

#[cfg(test)]
mod tests {
    use super::{GatewayPayload, Intents, OpCode};

    #[test]
    fn preserves_unknown_opcode() {
        let payload: GatewayPayload = serde_json::from_str(r#"{"op":255,"d":{"future":true}}"#)
            .expect("payload should decode");

        assert_eq!(payload.op, OpCode::new(255));
        assert_eq!(serde_json::to_value(payload.op).unwrap(), 255);
    }

    #[test]
    fn intents_use_numeric_wire_format_and_keep_unknown_bits() {
        assert_eq!(Intents::empty().with_open_forums().bits(), 1_u32 << 18);
        assert_eq!(
            Intents::empty().with_audio_live_members().bits(),
            1_u32 << 19
        );
        assert_eq!(Intents::empty().with_forums().bits(), 1_u32 << 28);
        assert_eq!(Intents::empty().with_audio_actions().bits(), 1_u32 << 29);

        let intents = Intents::empty()
            .with_guild_messages()
            .with_group_and_c2c()
            .with_public_guild_messages()
            .with_direct_messages()
            .with_interactions()
            .with_open_forums()
            .with_audio_live_members()
            .with_forums()
            .with_audio_actions();
        assert_eq!(
            serde_json::to_value(intents).unwrap(),
            (1_u32 << 9)
                | (1_u32 << 12)
                | (1_u32 << 18)
                | (1_u32 << 19)
                | (1_u32 << 25)
                | (1_u32 << 26)
                | (1_u32 << 28)
                | (1_u32 << 29)
                | (1_u32 << 30)
        );

        let decoded: Intents = serde_json::from_str("2147483648").unwrap();
        assert_eq!(decoded.bits(), 1_u32 << 31);
        assert_eq!(
            serde_json::to_value(Intents::GROUP_MEMBER_EVENT).unwrap(),
            1_u32 << 24
        );
    }
}
