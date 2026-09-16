//! Reading a PROXY protocol header from an upstream load balancer.
//!
//! Parsing is [`proxy_header`]'s job — it handles v1 and v2, TLVs and all the
//! edge cases. What stays here is the part that is a policy decision rather
//! than a parsing problem: a header is a *claim* about who is connecting, and
//! it is only believed when it comes from a peer the operator listed as
//! trusted.

use std::net::SocketAddr;

use proxy_header::{ParseConfig, ProxyHeader};

/// v2 headers start with this 12-byte signature, v1 headers with `PROXY `.
/// Checking the prefix ourselves is what separates "no header at all" from
/// "a malformed header", which the parser cannot tell apart.
const V2_SIGNATURE: [u8; 12] =
    [0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A];
const V1_PREFIX: &[u8] = b"PROXY ";

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("need more bytes")]
    Incomplete,
    #[error("no PROXY protocol header present")]
    NotPresent,
    #[error("malformed PROXY protocol header")]
    Malformed,
}

impl Error {
    pub fn is_incomplete(&self) -> bool {
        matches!(self, Error::Incomplete)
    }
}

/// Could these bytes still become a PROXY header?
///
/// Returns `false` as soon as the buffer diverges from both known prefixes, so
/// a Minecraft handshake is recognised as "no header" on its first byte.
pub fn could_be_header(buf: &[u8]) -> bool {
    let matches_prefix = |prefix: &[u8]| {
        let shared = buf.len().min(prefix.len());
        buf[..shared] == prefix[..shared]
    };
    buf.is_empty() || matches_prefix(&V2_SIGNATURE) || matches_prefix(V1_PREFIX)
}

/// Parses a header from the front of `buf`.
///
/// Returns the proxied source address — `None` for the `LOCAL` command, which
/// an upstream health check sends and which carries no client identity — and
/// how many bytes the header occupied.
pub fn parse(buf: &[u8]) -> Result<(Option<SocketAddr>, usize), Error> {
    if !could_be_header(buf) {
        return Err(Error::NotPresent);
    }

    // TLVs are not used here, and skipping them saves an allocation per
    // connection.
    let config = ParseConfig { include_tlvs: false, allow_v1: true, allow_v2: true };

    match ProxyHeader::parse(buf, config) {
        Ok((header, len)) => Ok((header.proxied_address().map(|addr| addr.source), len)),
        Err(proxy_header::Error::BufferTooShort) => Err(Error::Incomplete),
        Err(_) => Err(Error::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proxy_header::{ProxiedAddress, ProxyHeader};

    fn addr(value: &str) -> SocketAddr {
        value.parse().unwrap()
    }

    fn v2_header(src: &str, dst: &str) -> Vec<u8> {
        let header =
            ProxyHeader::with_address(ProxiedAddress::stream(addr(src), addr(dst)));
        let mut buf = vec![0u8; 128];
        let len = header.encode_to_slice_v2(&mut buf).expect("encodes");
        buf.truncate(len);
        buf
    }

    #[test]
    fn reads_a_v2_header() {
        let raw = v2_header("203.0.113.7:51234", "198.51.100.1:25565");
        let (source, len) = parse(&raw).unwrap();
        assert_eq!(source, Some(addr("203.0.113.7:51234")));
        assert_eq!(len, raw.len());
    }

    #[test]
    fn reads_a_v1_header_and_leaves_the_rest() {
        let raw = b"PROXY TCP4 203.0.113.7 198.51.100.1 51234 25565\r\nhandshake";
        let (source, len) = parse(raw).unwrap();
        assert_eq!(source, Some(addr("203.0.113.7:51234")));
        assert_eq!(&raw[len..], b"handshake");
    }

    #[test]
    fn a_local_header_carries_no_client() {
        let mut buf = vec![0u8; 64];
        let len = ProxyHeader::with_local().encode_to_slice_v2(&mut buf).unwrap();
        buf.truncate(len);
        assert_eq!(parse(&buf).unwrap(), (None, len));
    }

    #[test]
    fn partial_headers_ask_for_more_bytes() {
        let full = v2_header("203.0.113.7:51234", "198.51.100.1:25565");
        for cut in 1..full.len() {
            assert_eq!(parse(&full[..cut]), Err(Error::Incomplete), "cut at {cut}");
        }
        assert_eq!(parse(b"PROXY TCP4 203.0.113.7 "), Err(Error::Incomplete));
    }

    #[test]
    fn a_minecraft_handshake_is_no_header_rather_than_a_bad_one() {
        // Length 16, packet id 0, protocol 767... — nothing like either prefix.
        assert_eq!(parse(&[0x10, 0x00, 0xff, 0x05, 0x09]), Err(Error::NotPresent));
        assert!(!could_be_header(&[0x10, 0x00]));
        assert!(could_be_header(&[0x0D, 0x0A]), "a partial v2 signature stays possible");
        assert!(could_be_header(b"PRO"));
    }

    #[test]
    fn garbage_after_a_valid_prefix_is_malformed() {
        let mut raw = V2_SIGNATURE.to_vec();
        raw.extend_from_slice(&[0xFF; 20]);
        assert_eq!(parse(&raw), Err(Error::Malformed));
    }
}
