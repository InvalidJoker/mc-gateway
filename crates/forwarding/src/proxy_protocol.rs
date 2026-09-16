//! HAProxy PROXY protocol, versions 1 and 2.
//!
//! Outbound the gateway always speaks v2 (binary, fixed size, unambiguous).
//! Inbound both versions are accepted, because whatever sits in front of the
//! gateway is usually not configurable to the same degree.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

/// v2 signature: 12 bytes that cannot begin a valid Minecraft packet.
pub const V2_SIGNATURE: [u8; 12] =
    [0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A];
/// v1 prefix.
pub const V1_PREFIX: &[u8] = b"PROXY ";
/// Longest possible v1 line, per the specification.
pub const V1_MAX_LEN: usize = 107;
/// Longest header we will buffer before giving up (v2 with TLV extensions).
pub const MAX_HEADER_LEN: usize = 536;

const VERSION_2: u8 = 0x20;
const CMD_LOCAL: u8 = 0x00;
const CMD_PROXY: u8 = 0x01;
const AF_UNSPEC: u8 = 0x00;
const TCP4: u8 = 0x11;
const TCP6: u8 = 0x21;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyHeader {
    /// Sent by a health checker: the connection is from the proxy itself and
    /// carries no client identity.
    Local,
    /// A proxied connection with the original address pair.
    Proxy { src: SocketAddr, dst: SocketAddr },
}

impl ProxyHeader {
    pub fn source(&self) -> Option<SocketAddr> {
        match self {
            ProxyHeader::Local => None,
            ProxyHeader::Proxy { src, .. } => Some(*src),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("need more bytes")]
    Incomplete,
    #[error("no PROXY protocol header present")]
    NotPresent,
    #[error("malformed PROXY protocol header: {0}")]
    Malformed(&'static str),
    #[error("PROXY protocol header exceeds {MAX_HEADER_LEN} bytes")]
    TooLong,
}

impl ParseError {
    pub fn is_incomplete(&self) -> bool {
        matches!(self, ParseError::Incomplete)
    }
}

/// Writes a v2 PROXY header describing a real client connection.
pub fn encode_v2(src: SocketAddr, dst: SocketAddr) -> Vec<u8> {
    let mut out = Vec::with_capacity(28 + 12);
    out.extend_from_slice(&V2_SIGNATURE);
    out.push(VERSION_2 | CMD_PROXY);

    // A v4 and a v6 endpoint cannot share one header, so if the two ends
    // disagree both are expressed as IPv6.
    match (src.ip(), dst.ip()) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            out.push(TCP4);
            out.extend_from_slice(&12u16.to_be_bytes());
            out.extend_from_slice(&s.octets());
            out.extend_from_slice(&d.octets());
            out.extend_from_slice(&src.port().to_be_bytes());
            out.extend_from_slice(&dst.port().to_be_bytes());
        }
        (s, d) => {
            let s = to_v6(s);
            let d = to_v6(d);
            out.push(TCP6);
            out.extend_from_slice(&36u16.to_be_bytes());
            out.extend_from_slice(&s.octets());
            out.extend_from_slice(&d.octets());
            out.extend_from_slice(&src.port().to_be_bytes());
            out.extend_from_slice(&dst.port().to_be_bytes());
        }
    }

    out
}

/// Writes a v2 LOCAL header.
///
/// This is what the specification reserves for health checks: it tells the
/// backend "this connection is mine, not a client's", so proxy-protocol-only
/// listeners accept the probe without inventing a fake client address.
pub fn encode_v2_local() -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&V2_SIGNATURE);
    out.push(VERSION_2 | CMD_LOCAL);
    out.push(AF_UNSPEC);
    out.extend_from_slice(&0u16.to_be_bytes());
    out
}

fn to_v6(ip: IpAddr) -> Ipv6Addr {
    match ip {
        IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        IpAddr::V6(v6) => v6,
    }
}

/// Parses a header from the front of `buf`, returning it and its length.
pub fn parse(buf: &[u8]) -> Result<(ProxyHeader, usize), ParseError> {
    if buf.is_empty() {
        return Err(ParseError::Incomplete);
    }
    if buf.starts_with(&V2_SIGNATURE[..buf.len().min(V2_SIGNATURE.len())]) {
        return if buf.len() < V2_SIGNATURE.len() {
            Err(ParseError::Incomplete)
        } else {
            parse_v2(buf)
        };
    }
    if V1_PREFIX.starts_with(&buf[..buf.len().min(V1_PREFIX.len())]) {
        return parse_v1(buf);
    }
    Err(ParseError::NotPresent)
}

fn parse_v2(buf: &[u8]) -> Result<(ProxyHeader, usize), ParseError> {
    if buf.len() < 16 {
        return Err(ParseError::Incomplete);
    }
    let version_command = buf[12];
    if version_command & 0xF0 != VERSION_2 {
        return Err(ParseError::Malformed("unsupported version"));
    }
    let family = buf[13];
    let len = u16::from_be_bytes([buf[14], buf[15]]) as usize;
    let total = 16 + len;
    if total > MAX_HEADER_LEN {
        return Err(ParseError::TooLong);
    }
    if buf.len() < total {
        return Err(ParseError::Incomplete);
    }
    let body = &buf[16..total];

    match version_command & 0x0F {
        CMD_LOCAL => Ok((ProxyHeader::Local, total)),
        CMD_PROXY => match family {
            TCP4 => {
                if body.len() < 12 {
                    return Err(ParseError::Malformed("short IPv4 address block"));
                }
                let src_ip = <[u8; 4]>::try_from(&body[0..4]).expect("4 bytes");
                let dst_ip = <[u8; 4]>::try_from(&body[4..8]).expect("4 bytes");
                let src_port = u16::from_be_bytes([body[8], body[9]]);
                let dst_port = u16::from_be_bytes([body[10], body[11]]);
                Ok((
                    ProxyHeader::Proxy {
                        src: SocketAddr::from((src_ip, src_port)),
                        dst: SocketAddr::from((dst_ip, dst_port)),
                    },
                    total,
                ))
            }
            TCP6 => {
                if body.len() < 36 {
                    return Err(ParseError::Malformed("short IPv6 address block"));
                }
                let src_ip = <[u8; 16]>::try_from(&body[0..16]).expect("16 bytes");
                let dst_ip = <[u8; 16]>::try_from(&body[16..32]).expect("16 bytes");
                let src_port = u16::from_be_bytes([body[32], body[33]]);
                let dst_port = u16::from_be_bytes([body[34], body[35]]);
                Ok((
                    ProxyHeader::Proxy {
                        src: SocketAddr::from((src_ip, src_port)),
                        dst: SocketAddr::from((dst_ip, dst_port)),
                    },
                    total,
                ))
            }
            // AF_UNSPEC with the PROXY command: the sender knows nothing about
            // the original connection. Treat it like LOCAL.
            _ => Ok((ProxyHeader::Local, total)),
        },
        _ => Err(ParseError::Malformed("unknown command")),
    }
}

fn parse_v1(buf: &[u8]) -> Result<(ProxyHeader, usize), ParseError> {
    let end = match buf.windows(2).position(|w| w == b"\r\n") {
        Some(pos) => pos,
        None => {
            return if buf.len() >= V1_MAX_LEN {
                Err(ParseError::TooLong)
            } else {
                Err(ParseError::Incomplete)
            };
        }
    };
    let line = std::str::from_utf8(&buf[..end]).map_err(|_| ParseError::Malformed("not UTF-8"))?;
    let total = end + 2;

    let mut parts = line.split(' ');
    if parts.next() != Some("PROXY") {
        return Err(ParseError::NotPresent);
    }
    let family = parts.next().ok_or(ParseError::Malformed("missing family"))?;
    if family == "UNKNOWN" {
        return Ok((ProxyHeader::Local, total));
    }
    if family != "TCP4" && family != "TCP6" {
        return Err(ParseError::Malformed("unknown family"));
    }

    let mut next = || parts.next().ok_or(ParseError::Malformed("missing field"));
    let src_ip: IpAddr = next()?.parse().map_err(|_| ParseError::Malformed("bad source IP"))?;
    let dst_ip: IpAddr = next()?.parse().map_err(|_| ParseError::Malformed("bad dest IP"))?;
    let src_port: u16 = next()?.parse().map_err(|_| ParseError::Malformed("bad source port"))?;
    let dst_port: u16 = next()?.parse().map_err(|_| ParseError::Malformed("bad dest port"))?;

    Ok((
        ProxyHeader::Proxy {
            src: SocketAddr::new(src_ip, src_port),
            dst: SocketAddr::new(dst_ip, dst_port),
        },
        total,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn v2_ipv4_header_matches_the_specification_layout() {
        let header = encode_v2(addr("203.0.113.7:51234"), addr("198.51.100.1:25565"));
        assert_eq!(&header[..12], &V2_SIGNATURE);
        assert_eq!(header[12], 0x21, "version 2, PROXY command");
        assert_eq!(header[13], 0x11, "TCP over IPv4");
        assert_eq!(u16::from_be_bytes([header[14], header[15]]), 12);
        assert_eq!(header.len(), 28);
        assert_eq!(&header[16..20], &[203, 0, 113, 7]);
        assert_eq!(&header[24..26], &51234u16.to_be_bytes());
    }

    #[test]
    fn v2_roundtrips_for_both_families() {
        for (src, dst) in [
            ("203.0.113.7:51234", "198.51.100.1:25565"),
            ("[2001:db8::7]:51234", "[2001:db8::1]:25565"),
        ] {
            let (src, dst) = (addr(src), addr(dst));
            let encoded = encode_v2(src, dst);
            let (header, len) = parse(&encoded).unwrap();
            assert_eq!(len, encoded.len());
            assert_eq!(header, ProxyHeader::Proxy { src, dst });
        }
    }

    #[test]
    fn mixed_families_are_expressed_as_ipv6() {
        let encoded = encode_v2(addr("203.0.113.7:51234"), addr("[2001:db8::1]:25565"));
        assert_eq!(encoded[13], 0x21, "TCP over IPv6");
        let (header, _) = parse(&encoded).unwrap();
        let ProxyHeader::Proxy { src, .. } = header else { panic!("expected a PROXY header") };
        assert_eq!(src, addr("[::ffff:203.0.113.7]:51234"));
    }

    #[test]
    fn local_header_carries_no_identity() {
        let encoded = encode_v2_local();
        assert_eq!(encoded.len(), 16);
        let (header, len) = parse(&encoded).unwrap();
        assert_eq!(header, ProxyHeader::Local);
        assert_eq!(header.source(), None);
        assert_eq!(len, 16);
    }

    #[test]
    fn parses_v1_text_headers() {
        let raw = b"PROXY TCP4 203.0.113.7 198.51.100.1 51234 25565\r\nrest";
        let (header, len) = parse(raw).unwrap();
        assert_eq!(
            header,
            ProxyHeader::Proxy {
                src: addr("203.0.113.7:51234"),
                dst: addr("198.51.100.1:25565"),
            }
        );
        assert_eq!(&raw[len..], b"rest");

        let (unknown, _) = parse(b"PROXY UNKNOWN\r\n").unwrap();
        assert_eq!(unknown, ProxyHeader::Local);
    }

    #[test]
    fn partial_headers_ask_for_more_bytes() {
        let full = encode_v2(addr("203.0.113.7:51234"), addr("198.51.100.1:25565"));
        for cut in 1..full.len() {
            assert_eq!(parse(&full[..cut]), Err(ParseError::Incomplete), "cut at {cut}");
        }
        assert_eq!(parse(b"PROXY TCP4 203.0.113.7 "), Err(ParseError::Incomplete));
    }

    #[test]
    fn a_minecraft_handshake_is_recognised_as_having_no_header() {
        // Length 16, packet id 0, protocol 767...
        assert_eq!(parse(&[0x10, 0x00, 0xff, 0x05, 0x09]), Err(ParseError::NotPresent));
    }

    #[test]
    fn refuses_to_buffer_an_unbounded_header() {
        let mut bomb = V2_SIGNATURE.to_vec();
        bomb.push(0x21);
        bomb.push(0x11);
        bomb.extend_from_slice(&65535u16.to_be_bytes());
        assert_eq!(parse(&bomb), Err(ParseError::TooLong));

        let mut v1_bomb = b"PROXY TCP4 ".to_vec();
        v1_bomb.extend(std::iter::repeat_n(b'1', V1_MAX_LEN));
        assert_eq!(parse(&v1_bomb), Err(ParseError::TooLong));
    }
}
