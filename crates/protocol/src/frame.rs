use crate::{Error, Reader, Result, varint};

/// Hard cap on a single pre-login packet.
///
/// The protocol allows 2 MiB packets, but nothing sent before login comes close
/// to this. Anything larger from an unauthenticated peer is memory pressure we
/// have no reason to accept.
pub const MAX_PACKET_SIZE: usize = 32 * 1024;

/// One decoded length-prefixed packet, borrowed from the read buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame<'a> {
    /// Packet id (the first VarInt of the body).
    pub id: i32,
    /// Everything after the packet id.
    pub body: &'a [u8],
    /// Bytes this frame occupies in the source buffer, including the length
    /// prefix. Use this to know how much to drain, and what to replay verbatim.
    pub total_len: usize,
}

/// Decodes one uncompressed packet from the front of `buf`.
///
/// Pre-login traffic is never compressed or encrypted, which is exactly the
/// window in which the gateway inspects anything.
pub fn decode_frame(buf: &[u8]) -> Result<Frame<'_>> {
    decode_frame_limited(buf, MAX_PACKET_SIZE)
}

/// Like [`decode_frame`] but with a caller-chosen size cap.
///
/// Status responses read *from a backend* can legitimately be far larger than
/// anything a client may send — a favicon and a long player sample add up — and
/// that direction is trusted, so it gets its own limit.
pub fn decode_frame_limited(buf: &[u8], max_size: usize) -> Result<Frame<'_>> {
    let mut pos = 0usize;
    let length = varint::read_varint(buf, &mut pos)?;
    if length < 0 {
        return Err(Error::NegativeLength(length));
    }
    let length = length as usize;
    if length > max_size {
        return Err(Error::PacketTooLarge { max: max_size, actual: length });
    }
    let header_len = pos;
    if buf.len() < header_len + length {
        return Err(Error::Incomplete);
    }

    let payload = &buf[header_len..header_len + length];
    let mut id_pos = 0usize;
    let id = varint::read_varint(payload, &mut id_pos)?;

    Ok(Frame { id, body: &payload[id_pos..], total_len: header_len + length })
}

impl<'a> Frame<'a> {
    pub fn reader(&self) -> Reader<'a> {
        Reader::new(self.body)
    }

    pub fn expect_id(&self, expected: i32) -> Result<()> {
        if self.id == expected {
            Ok(())
        } else {
            Err(Error::UnexpectedPacket { expected, actual: self.id })
        }
    }
}

/// Encodes `body` as a packet with the given id: `len(id + body) | id | body`.
pub fn encode_packet(id: i32, body: &[u8]) -> Vec<u8> {
    let payload_len = varint::varint_len(id) + body.len();
    let mut out = Vec::with_capacity(varint::varint_len(payload_len as i32) + payload_len);
    varint::write_varint(&mut out, payload_len as i32);
    varint::write_varint(&mut out, id);
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_minimal_packet() {
        // len=1, id=0x00, empty body — a status request.
        let frame = decode_frame(&[0x01, 0x00]).unwrap();
        assert_eq!(frame.id, 0x00);
        assert!(frame.body.is_empty());
        assert_eq!(frame.total_len, 2);
    }

    #[test]
    fn leaves_trailing_bytes_for_the_next_frame() {
        let mut buf = encode_packet(0x00, &[]);
        buf.extend_from_slice(&encode_packet(0x01, &[1, 2, 3, 4, 5, 6, 7, 8]));
        let first = decode_frame(&buf).unwrap();
        assert_eq!(first.total_len, 2);
        let second = decode_frame(&buf[first.total_len..]).unwrap();
        assert_eq!(second.id, 0x01);
        assert_eq!(second.body, &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn partial_packets_are_incomplete() {
        let full = encode_packet(0x00, &[0xaa; 40]);
        for cut in 0..full.len() {
            assert_eq!(decode_frame(&full[..cut]), Err(Error::Incomplete), "cut at {cut}");
        }
        assert!(decode_frame(&full).is_ok());
    }

    #[test]
    fn rejects_a_declared_length_bomb_without_allocating() {
        // VarInt 10 MiB length prefix, no payload.
        let mut buf = Vec::new();
        varint::write_varint(&mut buf, 10 * 1024 * 1024);
        assert!(matches!(decode_frame(&buf), Err(Error::PacketTooLarge { .. })));
    }

    #[test]
    fn roundtrips_through_encode() {
        let encoded = encode_packet(0x7f, b"hello");
        let frame = decode_frame(&encoded).unwrap();
        assert_eq!(frame.id, 0x7f);
        assert_eq!(frame.body, b"hello");
        assert_eq!(frame.total_len, encoded.len());
    }
}
