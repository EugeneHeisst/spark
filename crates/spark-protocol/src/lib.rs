//! Shared wire contract for Spark's native service and Rust/WASM interface.
use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_LAN_PORT: u16 = 39393;
pub const DISCOVERY_PORT: u16 = 39394;
pub const MAX_JSON_BYTES: usize = 64 * 1024;
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_CHAT_CHARS: usize = 8000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub id: String,
    pub name: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub kind: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub voice_channel: Option<String>,
    pub muted: bool,
    pub camera: bool,
    pub screen: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileInfo {
    pub id: String,
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub mime: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessage {
    pub id: String,
    pub channel_id: String,
    pub sender_id: String,
    pub sender_name: String,
    pub text: String,
    pub created_at: i64,
    pub file: Option<FileInfo>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Discovery {
    pub id: String,
    pub name: String,
    pub address: String,
    pub peers: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectionInfo {
    pub server_name: String,
    pub address: String,
    pub hosting: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bootstrap {
    pub version: String,
    pub token: String,
    pub profile: Profile,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ClientCommand {
    Chat {
        channel_id: String,
        text: String,
    },
    CreateChannel {
        name: String,
        channel_kind: String,
    },
    JoinVoice {
        channel_id: String,
    },
    LeaveVoice,
    MediaState {
        muted: bool,
        camera: bool,
        screen: bool,
    },
    Signal {
        to: String,
        payload: serde_json::Value,
    },
    History {
        channel_id: String,
        before: Option<i64>,
    },
    Invite {
        to: String,
        channel_id: String,
        video: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ServerEvent {
    Welcome {
        self_id: String,
        server_name: String,
        channels: Vec<Channel>,
        peers: Vec<Peer>,
        messages: Vec<ChatMessage>,
    },
    PeerJoined {
        peer: Peer,
    },
    PeerLeft {
        peer_id: String,
    },
    Presence {
        peer: Peer,
    },
    ChannelCreated {
        channel: Channel,
    },
    Chat {
        message: ChatMessage,
    },
    Signal {
        from: String,
        payload: serde_json::Value,
    },
    History {
        channel_id: String,
        messages: Vec<ChatMessage>,
    },
    Invite {
        from: Peer,
        channel_id: String,
        video: bool,
    },
    Error {
        code: String,
        message: String,
    },
    Disconnected {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_and_events_use_the_frozen_wire_contract() {
        assert_eq!(
            serde_json::to_value(ClientCommand::CreateChannel {
                name: "Общий".into(),
                channel_kind: "text".into()
            })
            .unwrap(),
            serde_json::json!({"kind":"create_channel","name":"Общий","channel_kind":"text"})
        );
        let event: ServerEvent = serde_json::from_value(serde_json::json!({"kind":"signal","from":"peer","payload":{"type":"offer","sdp":"test"}})).unwrap();
        assert!(matches!(event, ServerEvent::Signal { from, .. } if from == "peer"));
        assert!(serde_json::from_str::<ClientCommand>(r#"{"kind":"unknown"}"#).is_err());
    }
}
