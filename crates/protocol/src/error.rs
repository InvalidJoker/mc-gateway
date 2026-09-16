use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Not an error in the usual sense: the caller must read more bytes and
    /// retry. Never log this as a failure.
    #[error("need more bytes")]
    Incomplete,

    #[error("VarInt is longer than 5 bytes")]
    VarIntTooLong,

    #[error("string too long: {actual} > {max}")]
    StringTooLong { max: usize, actual: usize },

    #[error("negative length prefix: {0}")]
    NegativeLength(i32),

    #[error("invalid UTF-8 in string field")]
    InvalidUtf8,

    #[error("packet of {actual} bytes exceeds the {max} byte limit")]
    PacketTooLarge { max: usize, actual: usize },

    #[error("expected packet id {expected:#04x}, got {actual:#04x}")]
    UnexpectedPacket { expected: i32, actual: i32 },

    #[error("unknown next_state {0} in handshake")]
    UnknownNextState(i32),

    /// The first bytes cannot start a Minecraft handshake. Usually a port
    /// scanner, an HTTP probe or a misconfigured client.
    #[error("not a Minecraft handshake")]
    NotMinecraft,
}

impl Error {
    /// True when more data could still turn this into a successful parse.
    pub fn is_incomplete(&self) -> bool {
        matches!(self, Error::Incomplete)
    }
}

/// Short, log-friendly category used as a metrics label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kind(pub &'static str);

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl Error {
    pub fn kind(&self) -> Kind {
        Kind(match self {
            Error::Incomplete => "incomplete",
            Error::VarIntTooLong => "varint_too_long",
            Error::StringTooLong { .. } => "string_too_long",
            Error::NegativeLength(_) => "negative_length",
            Error::InvalidUtf8 => "invalid_utf8",
            Error::PacketTooLarge { .. } => "packet_too_large",
            Error::UnexpectedPacket { .. } => "unexpected_packet",
            Error::UnknownNextState(_) => "unknown_next_state",
            Error::NotMinecraft => "not_minecraft",
        })
    }
}
