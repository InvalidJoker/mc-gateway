//! Handling one intercepted connection.
//!
//! Every connection to an intercepted port is someone else's server. The
//! gateway's whole job there is to be invisible, with one exception: a status
//! ping gets the node owner's MOTD line.
//!
//! That shapes every decision in this module:
//!
//! * The backend is connected **first**, before a byte from the client is read.
//!   A stopped server stays a closed port, and a protocol where the server
//!   speaks first is never kept waiting.
//! * Anything that is not recognisably a status ping — a login, RCON, a query
//!   tool, something that is not Minecraft at all — is piped through unchanged.
//! * When in doubt, pass through. Every failure while looking at the traffic
//!   degrades to "no ad on this connection", never to a broken connection.
//! * No rate limits, no idle timeout, no kick messages: those are the customer's
//!   server's business, not the node's.

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
    time::{self, timeout},
};
use tracing::{debug, error, info};

use crate::{
    config::Motd,
    motd, observe,
    protocol::{
        Handshake, NextState, STATUS_RESPONSE_ID, decode_frame, decode_frame_limited,
        encode_status_response, read_varint,
    },
    server::Gateway,
    transparent,
};

/// First byte of a pre-1.7 server list ping, which has no VarInt framing.
const LEGACY_PING: u8 = 0xFE;

/// The protocol's own ceiling for a packet. A modpack's status response lists
/// every mod and can be large; refusing it would cost the customer their MOTD.
const MAX_STATUS_RESPONSE: usize = 2 * 1024 * 1024 - 1;

/// A handshake can never be longer than this: protocol VarInt, a 255-character
/// address, a port and the next-state VarInt, plus framing. A longer declared
/// length means the client is not speaking Minecraft.
const MAX_HANDSHAKE_LEN: i32 = 1100;

/// Accepts intercepted connections until shutdown.
pub async fn run(
    listener: TcpListener,
    gateway: Arc<Gateway>,
    mut shutdown: watch::Receiver<bool>,
    sessions: mpsc::Sender<()>,
) {
    match listener.local_addr() {
        Ok(address) => info!(%address, "intercepting"),
        Err(err) => error!(%err, "intercept listener has no address"),
    }

    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    // With TPROXY the accepted socket's local address is the
                    // address the player actually dialled — after Docker's DNAT,
                    // so it is the container itself.
                    let Ok(original) = stream.local_addr() else { continue };
                    let gateway = Arc::clone(&gateway);
                    let token = sessions.clone();
                    tokio::spawn(async move {
                        handle(stream, peer, original, gateway).await;
                        drop(token);
                    });
                }
                Err(err) => {
                    debug!(%err, "intercept accept failed");
                    time::sleep(Duration::from_millis(50)).await;
                }
            },
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    info!("no longer intercepting");
                    return;
                }
            }
        }
    }
}

/// How an intercepted connection went, for the log.
#[derive(Debug)]
enum Outcome {
    /// The server could not be reached; the client was closed, as a closed
    /// port would have been.
    Unreachable(String),
    /// The client left before saying anything useful.
    ClientGone,
    /// Piped through; `rewritten` tells whether the MOTD was changed.
    Piped { kind: &'static str, rewritten: bool, bytes: Option<(u64, u64)> },
}

/// Handles one intercepted connection to `original`.
///
/// Public so tests can drive it without TPROXY: they pass the fake server's
/// address as `original` directly.
pub async fn handle(
    client: TcpStream,
    peer: SocketAddr,
    original: SocketAddr,
    gateway: Arc<Gateway>,
) {
    observe::connection_opened();
    let outcome = serve(client, peer, original, &gateway).await;
    observe::connection_closed();

    let client = gateway.log_addr(peer);
    match outcome {
        Outcome::Unreachable(reason) => {
            debug!(%client, server = %original, %reason, "server unreachable, client closed");
        }
        Outcome::ClientGone => debug!(%client, server = %original, "client left before speaking"),
        Outcome::Piped { kind, rewritten, bytes } => {
            let (to_server, to_client) = bytes.unwrap_or_default();
            debug!(
                %client,
                server = %original,
                kind,
                rewritten,
                to_server,
                to_client,
                "intercepted connection ended"
            );
        }
    }
}

async fn serve(
    mut client: TcpStream,
    peer: SocketAddr,
    original: SocketAddr,
    gateway: &Arc<Gateway>,
) -> Outcome {
    let config = gateway.config();
    let _ = client.set_nodelay(true);
    let _ = transparent::enable_keepalive(&client);

    let mut upstream = match transparent::connect(original, peer, config.timeouts.connect).await {
        Ok(upstream) => upstream,
        Err(err) => {
            observe::server_unreachable();
            return Outcome::Unreachable(err.to_string());
        }
    };
    let _ = transparent::enable_keepalive(&upstream);

    let motd = &config.motd;
    let mut kind = "passthrough";
    let mut rewritten = false;

    if motd.rewrites_anything() {
        let mut buf = Vec::with_capacity(512);
        match sniff(&mut client, &upstream, &mut buf, config.timeouts.handshake).await {
            Sniff::ClientGone => return Outcome::ClientGone,
            Sniff::PassThrough => {}
            Sniff::Status => {
                kind = "status";
                observe::status_request();
            }
        }

        // Whatever was read goes to the server first, byte for byte.
        if !buf.is_empty() && upstream.write_all(&buf).await.is_err() {
            return Outcome::Unreachable("write failed".into());
        }

        if kind == "status" {
            match relay_status(&mut client, &mut upstream, motd, config.timeouts.status).await {
                Ok(changed) => rewritten = changed,
                Err(_) => return Outcome::Piped { kind, rewritten, bytes: None },
            }
        }
    }

    // No idle timeout: this is someone else's server, and keepalives already
    // catch peers that vanished.
    let bytes = copy_bidirectional(&mut client, &mut upstream).await.ok();
    Outcome::Piped { kind, rewritten, bytes }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sniff {
    /// A status handshake followed by the complete status request.
    Status,
    /// Anything else: forward it untouched.
    PassThrough,
    /// The client closed or errored before anything could be decided.
    ClientGone,
}

/// Reads from the client until it is clear whether this is a status ping.
///
/// Gives up — and passes through — as soon as the server sends something,
/// which it would never do before a Minecraft client has spoken.
async fn sniff(
    client: &mut TcpStream,
    upstream: &TcpStream,
    buf: &mut Vec<u8>,
    deadline: Duration,
) -> Sniff {
    let work = async {
        loop {
            if let Some(decision) = classify(buf) {
                return decision;
            }
            let mut chunk = [0u8; 2048];
            let mut probe = [0u8; 1];
            tokio::select! {
                read = client.read(&mut chunk) => match read {
                    Ok(0) | Err(_) => return Sniff::ClientGone,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                },
                _ = upstream.peek(&mut probe) => return Sniff::PassThrough,
            }
        }
    };
    // A client that sends a partial packet and then waits is not a Minecraft
    // client either; let the two ends sort it out themselves.
    timeout(deadline, work).await.unwrap_or(Sniff::PassThrough)
}

/// Decides from the bytes so far. `None` means "need more".
fn classify(buf: &[u8]) -> Option<Sniff> {
    if buf.is_empty() {
        return None;
    }
    if buf[0] == LEGACY_PING {
        return Some(Sniff::PassThrough);
    }

    let mut pos = 0;
    match read_varint(buf, &mut pos) {
        Ok(length) if length <= 0 || length > MAX_HANDSHAKE_LEN => return Some(Sniff::PassThrough),
        Ok(_) => {}
        Err(err) if err.is_incomplete() => return None,
        Err(_) => return Some(Sniff::PassThrough),
    }
    // The handshake's packet id is always 0x00. Checking it as soon as it
    // arrives lets a short request in another protocol through at once, instead
    // of waiting for a "packet" that will never be completed.
    match buf.get(pos) {
        None => return None,
        Some(0x00) => {}
        Some(_) => return Some(Sniff::PassThrough),
    }

    let frame = match decode_frame(buf) {
        Ok(frame) => frame,
        Err(err) if err.is_incomplete() => return None,
        Err(_) => return Some(Sniff::PassThrough),
    };
    match Handshake::decode(&frame) {
        Ok(handshake) if handshake.next_state == NextState::Status => {}
        _ => return Some(Sniff::PassThrough),
    }

    // The server answers only once it has the status request, so it has to be
    // read here too — a real client sends it as a separate write.
    match decode_frame(&buf[frame.total_len..]) {
        Ok(_) => Some(Sniff::Status),
        Err(err) if err.is_incomplete() => None,
        Err(_) => Some(Sniff::PassThrough),
    }
}

/// Forwards the server's status response with the MOTD lines replaced.
///
/// Returns whether it changed anything. Every problem short of an I/O error
/// forwards the original bytes instead.
async fn relay_status(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    motd: &Motd,
    deadline: Duration,
) -> io::Result<bool> {
    let mut buf = Vec::with_capacity(8192);

    loop {
        let parsed = match decode_frame_limited(&buf, MAX_STATUS_RESPONSE) {
            Ok(frame) => {
                let json = (frame.id == STATUS_RESPONSE_ID)
                    .then(|| frame.reader().string(MAX_STATUS_RESPONSE).ok())
                    .flatten();
                Some((frame.total_len, json))
            }
            Err(err) if err.is_incomplete() => None,
            Err(_) => {
                client.write_all(&buf).await?;
                return Ok(false);
            }
        };

        if let Some((len, json)) = parsed {
            let replacement = json
                .and_then(|json| motd::rewrite_status(&json, motd))
                .map(|json| encode_status_response(&json));
            let changed = replacement.is_some();
            match replacement {
                Some(packet) => {
                    observe::motd_rewritten();
                    client.write_all(&packet).await?;
                }
                None => client.write_all(&buf[..len]).await?,
            }
            client.write_all(&buf[len..]).await?;
            return Ok(changed);
        }

        let mut chunk = [0u8; 8192];
        match timeout(deadline, upstream.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => {
                client.write_all(&buf).await?;
                return Ok(false);
            }
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(err)) => return Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Writer, encode_packet, write_varint};

    fn handshake(next_state: i32) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(767).string("node.example.net").u16(30123).varint(next_state);
        encode_packet(0x00, w.as_slice())
    }

    #[test]
    fn a_status_ping_is_recognised_only_once_the_request_is_there() {
        let mut buf = handshake(1);
        assert_eq!(classify(&buf), None, "handshake alone: wait for the request");
        buf.extend_from_slice(&encode_packet(0x00, &[]));
        assert_eq!(classify(&buf), Some(Sniff::Status));
    }

    #[test]
    fn a_login_passes_through_immediately() {
        assert_eq!(classify(&handshake(2)), Some(Sniff::PassThrough));
        assert_eq!(classify(&handshake(3)), Some(Sniff::PassThrough), "transfer too");
    }

    #[test]
    fn other_protocols_pass_through_without_waiting_for_more() {
        // HTTP, an RCON packet, a legacy ping, TLS, garbage.
        for bytes in [
            &b"GET / HTTP/1.1\r\nHost: x\r\n\r\n"[..],
            &[0x0a, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x03, 0x00][..],
            &[0xFE, 0x01][..],
            &[0x16, 0x03, 0x01, 0x02, 0x00, 0x01, 0x00, 0x01, 0xfc][..],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff][..],
        ] {
            let decision = classify(bytes);
            assert_ne!(decision, Some(Sniff::Status), "{bytes:02x?}");
        }
        assert_eq!(classify(&[0xFE, 0x01]), Some(Sniff::PassThrough));
    }

    #[test]
    fn a_huge_declared_length_is_not_a_handshake() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 5000);
        assert_eq!(classify(&buf), Some(Sniff::PassThrough));
    }

    #[test]
    fn a_short_request_in_another_protocol_is_decided_on_its_second_byte() {
        // `G` reads as a 71-byte length, but `E` cannot be a handshake's id.
        assert_eq!(classify(b"GE"), Some(Sniff::PassThrough));
        assert_eq!(classify(b"G"), None);
    }

    #[test]
    fn a_partial_length_prefix_waits() {
        assert_eq!(classify(&[0x80]), None);
    }
}
