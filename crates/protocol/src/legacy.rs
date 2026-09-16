//! Legacy server list ping, used by clients up to 1.6.4.
//!
//! These clients never send a VarInt-framed handshake, so the very first byte
//! has to be sniffed before the modern parser touches the buffer. Without this
//! an old client sees a connection reset instead of a MOTD.

use crate::chat;

/// First byte of every legacy ping.
pub const LEGACY_PING_BYTE: u8 = 0xFE;
/// Server -> client kick/disconnect packet, which doubles as the ping reply.
pub const LEGACY_KICK_ID: u8 = 0xFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyPing {
    /// Beta 1.8 - 1.3: a bare `0xFE`. Reply has no protocol/version fields.
    Pre13,
    /// 1.4 - 1.6: `0xFE 0x01`, optionally followed by a plugin message with the
    /// hostname the client used. The extended reply format applies.
    Post14,
}

/// Classifies the first bytes on a connection.
///
/// `None` means "this is not a legacy ping" — hand the buffer to the modern
/// parser. Legacy clients send so little that one byte is enough to decide.
pub fn detect(buf: &[u8]) -> Option<LegacyPing> {
    match *buf.first()? {
        LEGACY_PING_BYTE if buf.get(1) == Some(&0x01) => Some(LegacyPing::Post14),
        // A lone 0xFE is ambiguous until a second byte arrives or the client
        // stops sending; treat it as the old form so the socket does not hang.
        LEGACY_PING_BYTE => Some(LegacyPing::Pre13),
        _ => None,
    }
}

/// Builds the legacy reply: `0xFF`, a UTF-16 character count, then UTF-16BE.
pub fn encode_response(
    kind: LegacyPing,
    protocol: i32,
    version: &str,
    motd: &str,
    online: i64,
    max: i64,
) -> Vec<u8> {
    // Legacy clients split these fields on the delimiter, so it must not occur
    // inside any field.
    let motd = chat::strip_formatting(motd).replace(['\u{0}', '\u{a7}'], " ");
    let motd = motd.lines().next().unwrap_or("").trim().to_owned();

    let payload = match kind {
        LegacyPing::Post14 => {
            format!("\u{a7}1\u{0}{protocol}\u{0}{version}\u{0}{motd}\u{0}{online}\u{0}{max}")
        }
        LegacyPing::Pre13 => format!("{motd}\u{a7}{online}\u{a7}{max}"),
    };

    let utf16: Vec<u16> = payload.encode_utf16().collect();
    let mut out = Vec::with_capacity(3 + utf16.len() * 2);
    out.push(LEGACY_KICK_ID);
    out.extend_from_slice(&(utf16.len() as u16).to_be_bytes());
    for unit in utf16 {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_utf16_payload(packet: &[u8]) -> String {
        assert_eq!(packet[0], LEGACY_KICK_ID);
        let len = u16::from_be_bytes([packet[1], packet[2]]) as usize;
        let units: Vec<u16> = packet[3..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(units.len(), len, "declared length must match the payload");
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn detects_both_legacy_forms() {
        assert_eq!(detect(&[0xFE, 0x01, 0xFA]), Some(LegacyPing::Post14));
        assert_eq!(detect(&[0xFE, 0x00]), Some(LegacyPing::Pre13));
        assert_eq!(detect(&[0xFE]), Some(LegacyPing::Pre13));
    }

    #[test]
    fn ignores_modern_handshakes() {
        // A modern handshake starts with a length VarInt, never 0xFE.
        assert_eq!(detect(&[0x10, 0x00, 0xf7, 0x05]), None);
        assert_eq!(detect(&[]), None);
    }

    #[test]
    fn builds_the_1_6_reply() {
        let packet = encode_response(LegacyPing::Post14, 127, "1.21", "My Network", 5, 100);
        let text = decode_utf16_payload(&packet);
        let fields: Vec<&str> = text.split('\u{0}').collect();
        assert_eq!(fields, ["\u{a7}1", "127", "1.21", "My Network", "5", "100"]);
    }

    #[test]
    fn builds_the_pre_1_3_reply() {
        let packet = encode_response(LegacyPing::Pre13, 127, "1.21", "My Network", 5, 100);
        assert_eq!(decode_utf16_payload(&packet), "My Network\u{a7}5\u{a7}100");
    }

    #[test]
    fn sanitises_motd_so_fields_cannot_be_forged() {
        let packet =
            encode_response(LegacyPing::Post14, 127, "1.21", "\u{a7}aEvil\u{0}999\nsecond", 5, 100);
        let text = decode_utf16_payload(&packet);
        let fields: Vec<&str> = text.split('\u{0}').collect();
        assert_eq!(fields.len(), 6, "injected delimiters must not add fields");
        assert_eq!(fields[3], "Evil 999");
    }
}
