use crate::{Error, Frame, Reader, Result, Writer, encode_packet};

/// Packet id of the handshake, the only packet in the handshaking state.
pub const HANDSHAKE_ID: i32 = 0x00;

/// Protocol limit for the server address field.
const MAX_ADDRESS_CHARS: usize = 255;

/// What the client wants to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextState {
    /// Server list ping.
    Status,
    /// Normal join.
    Login,
    /// Server-initiated transfer, 1.20.5+. Routed exactly like `Login`.
    Transfer,
}

impl NextState {
    pub fn from_i32(value: i32) -> Result<Self> {
        match value {
            1 => Ok(NextState::Status),
            2 => Ok(NextState::Login),
            3 => Ok(NextState::Transfer),
            other => Err(Error::UnknownNextState(other)),
        }
    }

    pub const fn as_i32(self) -> i32 {
        match self {
            NextState::Status => 1,
            NextState::Login => 2,
            NextState::Transfer => 3,
        }
    }

    /// Both login and transfer end up connected to a backend.
    pub const fn is_join(self) -> bool {
        matches!(self, NextState::Login | NextState::Transfer)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            NextState::Status => "status",
            NextState::Login => "login",
            NextState::Transfer => "transfer",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub protocol_version: i32,
    /// The address field exactly as the client sent it, including any `\0`
    /// separated extra data. Use [`Handshake::hostname`] for routing.
    pub server_address: String,
    pub server_port: u16,
    pub next_state: NextState,
}

impl Handshake {
    pub fn decode(frame: &Frame<'_>) -> Result<Self> {
        frame.expect_id(HANDSHAKE_ID)?;
        let mut r = frame.reader();
        Self::decode_body(&mut r)
    }

    pub fn decode_body(r: &mut Reader<'_>) -> Result<Self> {
        let protocol_version = r.varint()?;
        let server_address = r.string(MAX_ADDRESS_CHARS)?;
        let server_port = r.u16()?;
        let next_state = NextState::from_i32(r.varint()?)?;
        Ok(Self { protocol_version, server_address, server_port, next_state })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(self.protocol_version)
            .string(&self.server_address)
            .u16(self.server_port)
            .varint(self.next_state.as_i32());
        encode_packet(HANDSHAKE_ID, w.as_slice())
    }

    /// The hostname the client actually typed, normalised for routing.
    ///
    /// Three things have to be stripped, and all three show up in the wild:
    ///
    /// * Forge and NeoForge append a marker (`\0FML\0`, `\0FML2\0`, `\0FML3\0`)
    /// * BungeeCord-style forwarding appends `\0ip\0uuid\0properties`
    /// * SRV resolution can leave a fully-qualified trailing dot
    ///
    /// Routing on the raw field would silently break every modded client.
    pub fn hostname(&self) -> String {
        let host = self.server_address.split('\0').next().unwrap_or("");
        let host = host.trim_end_matches('.');
        host.to_ascii_lowercase()
    }

    /// Everything after the first `\0`, if the client appended anything.
    pub fn extra_data(&self) -> Option<&str> {
        self.server_address.split_once('\0').map(|(_, rest)| rest)
    }

    /// True when the address carries a Forge/NeoForge modloader marker.
    pub fn is_modded_handshake(&self) -> bool {
        self.extra_data().is_some_and(|extra| {
            let extra = extra.trim_end_matches('\0');
            extra.starts_with("FML") || extra.starts_with("FORGE")
        })
    }

    /// Best-effort version name for logs only. An unknown protocol number is
    /// never an error — the gateway forwards it regardless.
    pub fn version_name(&self) -> Option<&'static str> {
        version_name(self.protocol_version)
    }
}

/// Display-only mapping of protocol numbers to release names.
///
/// Deliberately incomplete: new versions simply log as `protocol=<n>` and are
/// proxied like any other.
pub fn version_name(protocol: i32) -> Option<&'static str> {
    Some(match protocol {
        4 => "1.7.2-1.7.5",
        5 => "1.7.6-1.7.10",
        47 => "1.8.x",
        107..=110 => "1.9.x",
        210 => "1.10.x",
        315 | 316 => "1.11.x",
        335 | 338 | 340 => "1.12.x",
        393 | 401 | 404 => "1.13.x",
        477 | 480 | 485 | 490 | 498 => "1.14.x",
        573 | 575 | 578 => "1.15.x",
        735 | 736 | 751 | 753 | 754 => "1.16.x",
        755 | 756 => "1.17.x",
        757 | 758 => "1.18.x",
        759..=762 => "1.19.x",
        763..=766 => "1.20.x",
        767..=773 => "1.21.x",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::decode_frame;

    fn handshake(address: &str, next_state: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(767).string(address).u16(25565).varint(next_state);
        encode_packet(HANDSHAKE_ID, w.as_slice())
    }

    #[test]
    fn decodes_a_vanilla_handshake() {
        let bytes = handshake("survival.example.net", 2);
        let frame = decode_frame(&bytes).unwrap();
        let hs = Handshake::decode(&frame).unwrap();
        assert_eq!(hs.protocol_version, 767);
        assert_eq!(hs.server_port, 25565);
        assert_eq!(hs.next_state, NextState::Login);
        assert_eq!(hs.hostname(), "survival.example.net");
        assert!(!hs.is_modded_handshake());
        assert_eq!(hs.version_name(), Some("1.21.x"));
    }

    #[test]
    fn strips_forge_markers_from_the_routing_host() {
        for marker in ["\0FML\0", "\0FML2\0", "\0FML3\0", "\0FORGE"] {
            let bytes = handshake(&format!("modded.example.net{marker}"), 2);
            let frame = decode_frame(&bytes).unwrap();
            let hs = Handshake::decode(&frame).unwrap();
            assert_eq!(hs.hostname(), "modded.example.net", "marker {marker:?}");
            assert!(hs.is_modded_handshake(), "marker {marker:?}");
        }
    }

    #[test]
    fn strips_bungeecord_forwarding_payload() {
        let bytes = handshake("hub.example.net\x00203.0.113.7\x0011111111222233334444555555555555", 2);
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(Handshake::decode(&frame).unwrap().hostname(), "hub.example.net");
    }

    #[test]
    fn normalises_srv_trailing_dot_and_case() {
        let bytes = handshake("Survival.Example.NET.", 1);
        let frame = decode_frame(&bytes).unwrap();
        let hs = Handshake::decode(&frame).unwrap();
        assert_eq!(hs.hostname(), "survival.example.net");
        assert_eq!(hs.next_state, NextState::Status);
    }

    #[test]
    fn transfer_state_routes_like_login() {
        let bytes = handshake("example.net", 3);
        let frame = decode_frame(&bytes).unwrap();
        let hs = Handshake::decode(&frame).unwrap();
        assert_eq!(hs.next_state, NextState::Transfer);
        assert!(hs.next_state.is_join());
    }

    #[test]
    fn rejects_an_unknown_next_state() {
        let bytes = handshake("example.net", 9);
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(Handshake::decode(&frame), Err(Error::UnknownNextState(9)));
    }

    #[test]
    fn reencodes_byte_identically() {
        let bytes = handshake("modded.example.net\0FML2\0", 2);
        let frame = decode_frame(&bytes).unwrap();
        let hs = Handshake::decode(&frame).unwrap();
        assert_eq!(hs.encode(), bytes);
    }
}
