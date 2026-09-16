//! Login start (for the player name in logs) and login disconnect (so the
//! gateway can tell a player *why* it will not connect them).

use serde_json::Value;

use crate::{Frame, Result, Writer, encode_packet};

pub const LOGIN_START_ID: i32 = 0x00;
pub const LOGIN_DISCONNECT_ID: i32 = 0x00;

const MAX_NAME_CHARS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStart {
    pub name: String,
    /// Sent by 1.19.3+ clients. It is client-asserted and unauthenticated —
    /// useful for logs, never for authorisation.
    pub uuid: Option<u128>,
}

impl LoginStart {
    pub fn decode(frame: &Frame<'_>) -> Result<Self> {
        frame.expect_id(LOGIN_START_ID)?;
        let mut r = frame.reader();
        let name = r.string(MAX_NAME_CHARS)?;
        // Layout after the name changed repeatedly between 1.19 and 1.20.2.
        // Anything we fail to read is simply not reported.
        let uuid = r.uuid().ok();
        Ok(Self { name, uuid })
    }

    pub fn uuid_string(&self) -> Option<String> {
        self.uuid.map(|uuid| {
            let h = format!("{uuid:032x}");
            format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
        })
    }
}

/// Disconnect packet for the login state, carrying a chat component.
pub fn encode_disconnect(reason: &Value) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(&reason.to_string());
    encode_packet(LOGIN_DISCONNECT_ID, w.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chat, frame::decode_frame};

    #[test]
    fn decodes_a_modern_login_start() {
        let mut w = Writer::new();
        w.string("Notch").bytes(&0x0693_9A3B_1DFD_4E0F_8FBC_3D8C_2A7E_1B4Du128.to_be_bytes());
        let packet = encode_packet(LOGIN_START_ID, w.as_slice());
        let login = LoginStart::decode(&decode_frame(&packet).unwrap()).unwrap();
        assert_eq!(login.name, "Notch");
        assert_eq!(login.uuid_string().unwrap(), "06939a3b-1dfd-4e0f-8fbc-3d8c2a7e1b4d");
    }

    #[test]
    fn decodes_a_legacy_login_start_without_uuid() {
        let mut w = Writer::new();
        w.string("Notch");
        let packet = encode_packet(LOGIN_START_ID, w.as_slice());
        let login = LoginStart::decode(&decode_frame(&packet).unwrap()).unwrap();
        assert_eq!(login.name, "Notch");
        assert_eq!(login.uuid, None);
    }

    #[test]
    fn disconnect_carries_a_chat_component() {
        let packet = encode_disconnect(&chat::component("&cBackend offline"));
        let frame = decode_frame(&packet).unwrap();
        assert_eq!(frame.id, LOGIN_DISCONNECT_ID);
        let json: Value = serde_json::from_str(&frame.reader().string(262144).unwrap()).unwrap();
        assert_eq!(json["text"], "\u{a7}cBackend offline");
    }
}
