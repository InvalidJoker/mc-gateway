//! Legacy server list ping, used by clients up to 1.6.4.
//!
//! These clients never send a VarInt-framed handshake, so the very first byte
//! has to be sniffed before the modern parser touches the buffer. Detection is
//! all that happens here: the ping itself is forwarded to a backend, which
//! still knows how to answer it, and the reply is passed straight back.

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

#[cfg(test)]
mod tests {
    use super::*;

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

}
