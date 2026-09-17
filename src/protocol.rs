//! The slice of the Minecraft protocol the gateway reads: packet framing, the
//! handshake and the status response. All of it happens before login, so none
//! of it is ever compressed or encrypted.

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Not a failure: read more bytes and try again.
    #[error("need more bytes")]
    Incomplete,
    #[error("VarInt is longer than 5 bytes")]
    VarIntTooLong,
    #[error("negative length prefix: {0}")]
    NegativeLength(i32),
    #[error("string too long: {actual} > {max}")]
    StringTooLong { max: usize, actual: usize },
    #[error("invalid UTF-8 in string field")]
    InvalidUtf8,
    #[error("packet of {actual} bytes exceeds the {max} byte limit")]
    PacketTooLarge { max: usize, actual: usize },
    #[error("expected packet id {expected:#04x}, got {actual:#04x}")]
    UnexpectedPacket { expected: i32, actual: i32 },
    #[error("unknown next_state {0} in handshake")]
    UnknownNextState(i32),
}

impl Error {
    pub fn is_incomplete(&self) -> bool {
        matches!(self, Error::Incomplete)
    }
}

// ------------------------------------------------------------------ varint --

/// Reads a VarInt at `*pos`, advancing `pos` past it.
pub fn read_varint(buf: &[u8], pos: &mut usize) -> Result<i32> {
    let mut value: u32 = 0;
    let mut shift = 0u32;
    let mut cursor = *pos;
    loop {
        let byte = *buf.get(cursor).ok_or(Error::Incomplete)?;
        cursor += 1;
        value |= u32::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 32 {
            return Err(Error::VarIntTooLong);
        }
    }
    *pos = cursor;
    Ok(value as i32)
}

pub fn write_varint(out: &mut Vec<u8>, value: i32) {
    let mut value = value as u32;
    loop {
        if value & !0x7F == 0 {
            out.push(value as u8);
            return;
        }
        out.push((value as u8 & 0x7F) | 0x80);
        value >>= 7;
    }
}

// ----------------------------------------------------------- reader/writer --

/// Reads fields from a packet body; every read reports [`Error::Incomplete`]
/// instead of panicking when the data runs out.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            return Err(Error::Incomplete);
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn varint(&mut self) -> Result<i32> {
        read_varint(self.buf, &mut self.pos)
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.take(8)?.try_into().expect("8 bytes")))
    }

    /// Length-prefixed UTF-8 string of at most `max_chars` characters.
    pub fn string(&mut self, max_chars: usize) -> Result<String> {
        let len = self.varint()?;
        if len < 0 {
            return Err(Error::NegativeLength(len));
        }
        let len = len as usize;
        if len > max_chars * 4 {
            return Err(Error::StringTooLong { max: max_chars * 4, actual: len });
        }
        let s = std::str::from_utf8(self.take(len)?).map_err(|_| Error::InvalidUtf8)?;
        if s.chars().count() > max_chars {
            return Err(Error::StringTooLong { max: max_chars, actual: s.chars().count() });
        }
        Ok(s.to_owned())
    }
}

/// Builds a packet body.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn varint(&mut self, value: i32) -> &mut Self {
        write_varint(&mut self.buf, value);
        self
    }

    pub fn u16(&mut self, value: u16) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn i64(&mut self, value: i64) -> &mut Self {
        self.buf.extend_from_slice(&value.to_be_bytes());
        self
    }

    pub fn bytes(&mut self, value: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(value);
        self
    }

    pub fn string(&mut self, value: &str) -> &mut Self {
        self.varint(value.len() as i32);
        self.bytes(value.as_bytes())
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

// ------------------------------------------------------------------ frames --

/// Size cap for packets read from a client. Nothing a client sends before login
/// comes close.
pub const MAX_PACKET_SIZE: usize = 32 * 1024;

/// One length-prefixed packet, borrowed from the read buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame<'a> {
    pub id: i32,
    /// Everything after the packet id.
    pub body: &'a [u8],
    /// Bytes the frame occupies in the buffer, length prefix included.
    pub total_len: usize,
}

impl<'a> Frame<'a> {
    pub fn reader(&self) -> Reader<'a> {
        Reader::new(self.body)
    }
}

pub fn decode_frame(buf: &[u8]) -> Result<Frame<'_>> {
    decode_frame_limited(buf, MAX_PACKET_SIZE)
}

/// Decodes one packet with a caller-chosen size cap.
pub fn decode_frame_limited(buf: &[u8], max_size: usize) -> Result<Frame<'_>> {
    let mut pos = 0usize;
    let length = read_varint(buf, &mut pos)?;
    if length < 0 {
        return Err(Error::NegativeLength(length));
    }
    let length = length as usize;
    if length > max_size {
        return Err(Error::PacketTooLarge { max: max_size, actual: length });
    }
    if buf.len() < pos + length {
        return Err(Error::Incomplete);
    }
    let payload = &buf[pos..pos + length];
    let mut id_pos = 0usize;
    let id = read_varint(payload, &mut id_pos)?;
    Ok(Frame { id, body: &payload[id_pos..], total_len: pos + length })
}

/// `len(id + body) | id | body`
pub fn encode_packet(id: i32, body: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(5 + body.len());
    write_varint(&mut payload, id);
    payload.extend_from_slice(body);
    let mut out = Vec::with_capacity(5 + payload.len());
    write_varint(&mut out, payload.len() as i32);
    out.extend_from_slice(&payload);
    out
}

// --------------------------------------------------------------- handshake --

pub const HANDSHAKE_ID: i32 = 0x00;
pub const STATUS_REQUEST_ID: i32 = 0x00;
pub const STATUS_RESPONSE_ID: i32 = 0x00;
pub const PING_ID: i32 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextState {
    Status,
    Login,
    /// Server-initiated transfer, 1.20.5+.
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub protocol_version: i32,
    pub server_address: String,
    pub server_port: u16,
    pub next_state: NextState,
}

impl Handshake {
    pub fn decode(frame: &Frame<'_>) -> Result<Self> {
        if frame.id != HANDSHAKE_ID {
            return Err(Error::UnexpectedPacket { expected: HANDSHAKE_ID, actual: frame.id });
        }
        let mut r = frame.reader();
        Ok(Self {
            protocol_version: r.varint()?,
            server_address: r.string(255)?,
            server_port: r.u16()?,
            next_state: NextState::from_i32(r.varint()?)?,
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(self.protocol_version)
            .string(&self.server_address)
            .u16(self.server_port)
            .varint(self.next_state.as_i32());
        encode_packet(HANDSHAKE_ID, w.as_slice())
    }
}

/// Packet id of the disconnect packet in the login state.
pub const LOGIN_DISCONNECT_ID: i32 = 0x00;

/// A login-state disconnect carrying a JSON chat component. The login state
/// uses JSON text in every protocol version, unlike later states.
pub fn encode_login_disconnect(reason_json: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(reason_json);
    encode_packet(LOGIN_DISCONNECT_ID, w.as_slice())
}

/// A status response packet carrying `json`.
pub fn encode_status_response(json: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.string(json);
    encode_packet(STATUS_RESPONSE_ID, w.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_match_the_protocol_reference_vectors() {
        let cases: &[(i32, &[u8])] = &[
            (0, &[0x00]),
            (127, &[0x7f]),
            (128, &[0x80, 0x01]),
            (25565, &[0xdd, 0xc7, 0x01]),
            (2147483647, &[0xff, 0xff, 0xff, 0xff, 0x07]),
            (-1, &[0xff, 0xff, 0xff, 0xff, 0x0f]),
        ];
        for (value, encoded) in cases {
            let mut out = Vec::new();
            write_varint(&mut out, *value);
            assert_eq!(&out, encoded, "encoding {value}");
            let mut pos = 0;
            assert_eq!(read_varint(encoded, &mut pos).unwrap(), *value);
            assert_eq!(pos, encoded.len());
        }
    }

    #[test]
    fn truncated_input_is_incomplete_and_oversized_varints_are_errors() {
        assert_eq!(read_varint(&[0xdd, 0xc7], &mut 0), Err(Error::Incomplete));
        assert_eq!(read_varint(&[0xff; 6], &mut 0), Err(Error::VarIntTooLong));
    }

    #[test]
    fn every_partial_frame_is_incomplete() {
        let full = encode_packet(0x00, &[0xaa; 40]);
        for cut in 0..full.len() {
            assert_eq!(decode_frame(&full[..cut]), Err(Error::Incomplete), "cut at {cut}");
        }
        assert_eq!(decode_frame(&full).unwrap().total_len, full.len());
    }

    #[test]
    fn a_declared_length_bomb_is_refused_without_allocating() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 10 * 1024 * 1024);
        assert!(matches!(decode_frame(&buf), Err(Error::PacketTooLarge { .. })));
    }

    #[test]
    fn handshakes_roundtrip() {
        for next_state in [NextState::Status, NextState::Login, NextState::Transfer] {
            let handshake = Handshake {
                protocol_version: 767,
                server_address: "node.example.net\0FML2\0".into(),
                server_port: 30123,
                next_state,
            };
            let bytes = handshake.encode();
            assert_eq!(Handshake::decode(&decode_frame(&bytes).unwrap()).unwrap(), handshake);
        }
    }

    #[test]
    fn an_unknown_next_state_is_rejected() {
        let mut w = Writer::new();
        w.varint(767).string("x").u16(1).varint(9);
        let bytes = encode_packet(HANDSHAKE_ID, w.as_slice());
        assert_eq!(Handshake::decode(&decode_frame(&bytes).unwrap()), Err(Error::UnknownNextState(9)));
    }

    #[test]
    fn a_status_response_roundtrips() {
        let packet = encode_status_response(r#"{"description":"hi"}"#);
        let frame = decode_frame(&packet).unwrap();
        assert_eq!(frame.id, STATUS_RESPONSE_ID);
        assert_eq!(frame.reader().string(1000).unwrap(), r#"{"description":"hi"}"#);
    }
}
