//! The smallest possible slice of the Minecraft protocol.
//!
//! The gateway is an L4 proxy first. It only decodes what it needs for routing
//! and for answering status pings itself:
//!
//! * packet framing (uncompressed, unencrypted — always true before login)
//! * the handshake packet
//! * the status request/ping exchange
//! * the legacy (<= 1.6) ping handshake
//! * enough of login start to log a player name and to send a disconnect
//!
//! Everything after the handshake is forwarded byte-for-byte, so compression,
//! encryption and any modloader-specific traffic pass through untouched.

pub mod chat;
pub mod error;
pub mod frame;
pub mod handshake;
pub mod legacy;
pub mod login;
pub mod status;
pub mod varint;

pub use error::{Error, Result};
pub use frame::{Frame, MAX_PACKET_SIZE, decode_frame, decode_frame_limited, encode_packet};
pub use handshake::{Handshake, NextState};
pub use varint::{read_varint, varint_len, write_varint};

/// Reader over a byte slice that knows how to stop when it runs out of data.
///
/// Every read returns [`Error::Incomplete`] instead of panicking, which lets the
/// session loop call the parser again after reading more from the socket.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(Error::Incomplete);
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    pub fn i64(&mut self) -> Result<i64> {
        let b = self.take(8)?;
        Ok(i64::from_be_bytes(b.try_into().expect("8 bytes")))
    }

    pub fn uuid(&mut self) -> Result<u128> {
        let b = self.take(16)?;
        Ok(u128::from_be_bytes(b.try_into().expect("16 bytes")))
    }

    pub fn varint(&mut self) -> Result<i32> {
        varint::read_varint(self.buf, &mut self.pos)
    }

    /// Length-prefixed UTF-8 string. `max_chars` follows the protocol's own
    /// limits; the byte budget is 4x that plus the length prefix.
    pub fn string(&mut self, max_chars: usize) -> Result<String> {
        let len = self.varint()?;
        if len < 0 {
            return Err(Error::NegativeLength(len));
        }
        let len = len as usize;
        if len > max_chars * 4 {
            return Err(Error::StringTooLong { max: max_chars * 4, actual: len });
        }
        let bytes = self.take(len)?;
        let s = std::str::from_utf8(bytes).map_err(|_| Error::InvalidUtf8)?;
        if s.chars().count() > max_chars {
            return Err(Error::StringTooLong { max: max_chars, actual: s.chars().count() });
        }
        Ok(s.to_owned())
    }
}

/// Growable packet body writer.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn varint(&mut self, value: i32) -> &mut Self {
        varint::write_varint(&mut self.buf, value);
        self
    }

    pub fn u8(&mut self, value: u8) -> &mut Self {
        self.buf.push(value);
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

    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}
