//! The status (server list ping) exchange.
//!
//! Client: handshake(next_state=1) -> status request 0x00 -> ping 0x01.
//! Server: status response 0x00 (JSON) -> pong 0x01 (echoes the payload).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Writer, encode_packet};

pub const STATUS_REQUEST_ID: i32 = 0x00;
pub const PING_REQUEST_ID: i32 = 0x01;
pub const STATUS_RESPONSE_ID: i32 = 0x00;
pub const PONG_RESPONSE_ID: i32 = 0x01;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerStatus {
    pub version: VersionInfo,
    pub players: PlayerInfo,
    pub description: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    /// 1.19.1+ clients show a warning toast when this is absent.
    #[serde(
        rename = "enforcesSecureChat",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub enforces_secure_chat: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub name: String,
    /// Reporting the client's own protocol number keeps the server from being
    /// drawn with the red "incompatible" cross in the server list.
    pub protocol: i32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerInfo {
    pub max: i64,
    pub online: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample: Vec<PlayerSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerSample {
    pub name: String,
    pub id: String,
}

impl PlayerSample {
    /// Hover-text line. The all-zero UUID is what servers conventionally use
    /// for entries that are not real players.
    pub fn line(text: impl Into<String>) -> Self {
        Self { name: text.into(), id: "00000000-0000-0000-0000-000000000000".into() }
    }
}

impl ServerStatus {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("status serialises")
    }
}

/// Status response packet carrying the JSON document.
pub fn encode_status_response(json: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(json);
    encode_packet(STATUS_RESPONSE_ID, w.as_slice())
}

/// Pong. The payload must be echoed back unchanged — the client measures
/// latency from it.
pub fn encode_pong(payload: i64) -> Vec<u8> {
    let mut w = Writer::new();
    w.i64(payload);
    encode_packet(PONG_RESPONSE_ID, w.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Reader, chat, frame::decode_frame};

    fn sample() -> ServerStatus {
        ServerStatus {
            version: VersionInfo { name: "MyNetwork".into(), protocol: 767 },
            players: PlayerInfo { max: 1000, online: 123, sample: vec![] },
            description: chat::component("&bMy Network"),
            favicon: None,
            enforces_secure_chat: Some(false),
        }
    }

    #[test]
    fn serialises_the_documented_shape() {
        let json: Value = serde_json::from_str(&sample().to_json()).unwrap();
        assert_eq!(json["version"]["protocol"], 767);
        assert_eq!(json["players"]["online"], 123);
        assert_eq!(json["players"]["max"], 1000);
        assert_eq!(json["enforcesSecureChat"], false);
        assert!(json.get("favicon").is_none(), "absent favicon must be omitted");
        assert!(json["description"]["text"].as_str().unwrap().contains("My Network"));
    }

    #[test]
    fn status_response_roundtrips_through_a_frame() {
        let json = sample().to_json();
        let packet = encode_status_response(&json);
        let frame = decode_frame(&packet).unwrap();
        assert_eq!(frame.id, STATUS_RESPONSE_ID);
        assert_eq!(frame.reader().string(32767).unwrap(), json);
    }

    #[test]
    fn pong_echoes_the_payload() {
        let packet = encode_pong(-8_070_430_567_881_151_616);
        let frame = decode_frame(&packet).unwrap();
        assert_eq!(frame.id, PONG_RESPONSE_ID);
        let mut r: Reader<'_> = frame.reader();
        assert_eq!(r.i64().unwrap(), -8_070_430_567_881_151_616);
    }
}
