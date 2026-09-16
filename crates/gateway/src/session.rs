//! One client connection, from `accept()` to close.
//!
//! The state machine is deliberately short: read the handshake, pick a backend,
//! and then get out of the way. The gateway is a pass-through proxy — even a
//! status ping is answered by the backend, with at most two lines of its MOTD
//! swapped out on the way back.

use std::{io, net::SocketAddr, sync::Arc, time::Duration};

use mc_config::{Config, InboundProxyProtocol, Motd};
use mc_forwarding::{Origin, inbound};
use mc_protocol::{
    Error as ProtoError, Frame, Handshake, NextState, chat,
    frame::{decode_frame, decode_frame_limited},
    legacy, login, status,
};
use mc_routing::{Route, SelectError};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, timeout},
};
use tracing::{debug, info, warn};

use crate::{
    app::{App, ListenerRuntime},
    limits::Rejection,
    motd, observe, pipe,
};

/// Backend status responses carry favicons and player samples, so they get a
/// larger budget than anything accepted from a client.
const MAX_STATUS_RESPONSE: usize = 256 * 1024;

/// How a connection ended, for logs and metrics.
#[derive(Debug)]
enum Outcome {
    /// Proxied to a backend. `kind` is `session`, `status` or `legacy`.
    Proxied {
        backend: String,
        kind: &'static str,
        seconds: u64,
        bytes: Option<(u64, u64)>,
    },
    /// Answered without a backend, because there was none to ask.
    Offline,
    /// Refused, with the metrics reason label.
    Refused(&'static str),
    /// The peer went away or misbehaved.
    Dropped(&'static str),
}

/// Handles one accepted connection. Never returns an error: everything is
/// logged and counted here, because there is no caller that could do better.
pub async fn serve(
    stream: TcpStream,
    peer: SocketAddr,
    listener: Arc<ListenerRuntime>,
    app: Arc<App>,
) {
    observe::connection_opened();
    let started = Instant::now();

    let outcome = run(stream, peer, &listener, &app).await;
    observe::connection_closed();

    let client = app.log_addr(peer);
    match outcome {
        Outcome::Proxied { backend, kind, seconds, bytes } => {
            observe::session_ended(&backend, bytes);
            let (to_backend, to_client) = bytes.unwrap_or_default();
            info!(
                listener = %listener.name,
                client = %client,
                %backend,
                kind,
                seconds,
                to_backend,
                to_client,
                clean = bytes.is_some(),
                "session ended"
            );
        }
        Outcome::Offline => {
            debug!(listener = %listener.name, client = %client, "answered with the offline MOTD");
        }
        Outcome::Refused(reason) => {
            observe::connection_rejected(reason);
            debug!(
                listener = %listener.name,
                client = %client,
                reason,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "connection refused"
            );
        }
        Outcome::Dropped(reason) => {
            debug!(listener = %listener.name, client = %client, reason, "connection dropped");
        }
    }
}

async fn run(
    mut stream: TcpStream,
    peer: SocketAddr,
    listener: &Arc<ListenerRuntime>,
    app: &Arc<App>,
) -> Outcome {
    // Small packets, latency-sensitive protocol.
    let _ = stream.set_nodelay(true);

    let runtime = app.runtime();
    let config = runtime.registry.config();
    let mut buf = Vec::with_capacity(512);

    // ---- 1. inbound PROXY protocol, if an upstream L4 hop is trusted -------
    let client = match read_inbound_header(
        &mut stream,
        &mut buf,
        &listener.proxy_protocol,
        peer,
        config.timeouts.handshake,
    )
    .await
    {
        Ok(addr) => addr,
        Err(reason) => return Outcome::Refused(reason),
    };

    // ---- 2. admission control, before anything is parsed ------------------
    let _permit = match app.limiter.admit(client.ip()) {
        Ok(permit) => permit,
        Err(rejection) => {
            // A rate-limited client gets a reason rather than a silent reset,
            // but only after the cheap checks have already passed.
            if rejection == Rejection::RateLimited {
                let _ = kick(&mut stream, &config.messages.rate_limited).await;
            }
            return Outcome::Refused(rejection.as_str());
        }
    };

    // ---- 3. the handshake, or a pre-1.7 ping ------------------------------
    let (handshake, handshake_len) = match read_handshake(&mut stream, &mut buf, config).await {
        Ok(HandshakeOutcome::Modern(handshake, len)) => (handshake, len),
        Ok(HandshakeOutcome::Legacy) => {
            // Old clients send no hostname, so there is nothing to route on.
            // Their ping is forwarded to the default target, which still knows
            // how to answer it.
            observe::status_request("legacy");
            let Some(default) = config.routing.default.as_deref() else {
                return Outcome::Dropped("legacy ping with no default route");
            };
            let Some(target) = runtime.registry.target(default) else {
                return Outcome::Dropped("legacy ping with no default route");
            };
            return match target.select() {
                Ok(backend) => {
                    proxy(stream, buf, &backend, config, client, "legacy", None).await
                }
                Err(_) => Outcome::Dropped("no backend for the legacy ping"),
            };
        }
        Err(outcome) => return outcome,
    };

    let host = handshake.hostname();
    let route = runtime.registry.route(&host, &listener.name);
    let motd =
        config.motd.overlay(route.as_ref().and_then(|r| r.rule).and_then(|r| r.motd.as_ref()));

    match handshake.next_state {
        NextState::Status => {
            serve_status(stream, buf, handshake_len, &handshake, route, &motd, config, client).await
        }
        NextState::Login | NextState::Transfer => {
            serve_login(stream, buf, handshake_len, &handshake, route, client, app, config).await
        }
    }
}

// ----------------------------------------------------------------- status --

#[allow(clippy::too_many_arguments, reason = "one connection's full context")]
async fn serve_status(
    mut stream: TcpStream,
    mut buf: Vec<u8>,
    handshake_len: usize,
    handshake: &Handshake,
    route: Option<Route<'_>>,
    motd: &Motd,
    config: &Arc<Config>,
    client: SocketAddr,
) -> Outcome {
    observe::status_request("status");

    let backend = route.as_ref().and_then(|route| route.target.select().ok());

    let Some(backend) = backend else {
        // Nothing to pass through: no route, or every member is down. This is
        // the only case where the gateway invents a status response.
        buf.drain(..handshake_len);
        let json = motd::offline_status(motd, handshake.protocol_version);
        return match answer_status(&mut stream, &mut buf, &json, config.timeouts.status).await {
            Ok(()) => Outcome::Offline,
            Err(reason) => Outcome::Dropped(reason),
        };
    };

    let rewrite = motd.rewrites_anything().then(|| motd.clone());
    proxy(stream, buf, &backend, config, client, "status", rewrite).await
}

/// Status request -> response, then the optional ping -> pong, answered here.
async fn answer_status(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    json: &str,
    deadline: Duration,
) -> Result<(), &'static str> {
    let request = read_frame(stream, buf, deadline, mc_protocol::MAX_PACKET_SIZE)
        .await
        .map_err(|_| "no status request")?;
    if request.id != status::STATUS_REQUEST_ID {
        return Err("unexpected packet in status state");
    }
    let consumed = request.total_len;
    buf.drain(..consumed);

    stream
        .write_all(&status::encode_status_response(json))
        .await
        .map_err(|_| "status write failed")?;

    // Many clients follow up with a ping to measure latency; some just close.
    let Ok(ping) = read_frame(stream, buf, deadline, mc_protocol::MAX_PACKET_SIZE).await else {
        return Ok(());
    };
    if ping.id != status::PING_REQUEST_ID {
        return Ok(());
    }
    let payload = ping.reader().i64().unwrap_or(0);
    let consumed = ping.total_len;
    buf.drain(..consumed);

    stream.write_all(&status::encode_pong(payload)).await.map_err(|_| "pong write failed")?;
    let _ = stream.flush().await;
    Ok(())
}

// ------------------------------------------------------------------ login --

#[allow(clippy::too_many_arguments, reason = "one connection's full context")]
async fn serve_login(
    mut stream: TcpStream,
    buf: Vec<u8>,
    handshake_len: usize,
    handshake: &Handshake,
    route: Option<Route<'_>>,
    client: SocketAddr,
    app: &Arc<App>,
    config: &Arc<Config>,
) -> Outcome {
    observe::login_attempt();

    // The login start packet often arrives in the same TCP segment. If it has,
    // the player name is free; if not, waiting for it would only add latency.
    let player = decode_frame(&buf[handshake_len..])
        .ok()
        .filter(|frame| frame.id == login::LOGIN_START_ID)
        .and_then(|frame| login::LoginStart::decode(&frame).ok())
        .map(|start| start.name);

    let Some(route) = route else {
        debug!(host = %handshake.hostname(), "no route for host");
        let _ = kick(&mut stream, &config.messages.no_route).await;
        return Outcome::Refused("no_route");
    };

    let backend = match route.target.select() {
        Ok(backend) => backend,
        Err(err) => {
            let (message, reason) = match err {
                SelectError::AllFull => (&config.messages.too_many_connections, "target_full"),
                _ => (&config.messages.backend_offline, "no_backend"),
            };
            warn!(target = %route.target.name, error = %err, "cannot select a backend");
            let _ = kick(&mut stream, message).await;
            return Outcome::Refused(reason);
        }
    };

    let Some(guard) = backend.try_acquire() else {
        let _ = kick(&mut stream, &config.messages.too_many_connections).await;
        return Outcome::Refused("target_full");
    };

    info!(
        client = %app.log_addr(client),
        host = %handshake.hostname(),
        player = player.as_deref().unwrap_or("?"),
        protocol = handshake.protocol_version,
        version = handshake.version_name().unwrap_or("unknown"),
        target = %route.target.name,
        backend = %backend.name,
        "routing player"
    );

    let outcome = proxy(stream, buf, &backend, config, client, "session", None).await;
    drop(guard);
    outcome
}

// ------------------------------------------------------------------ proxy --

/// Connects to the backend, replays what was read, and hands over.
///
/// `rewrite` turns on MOTD interception: the backend's status response is
/// parsed, its description lines are replaced and it is re-encoded. Everything
/// else about the response, and every other byte in either direction, is
/// forwarded untouched.
#[allow(clippy::too_many_arguments, reason = "one connection's full context")]
async fn proxy(
    mut stream: TcpStream,
    replay: Vec<u8>,
    backend: &Arc<mc_routing::Backend>,
    config: &Arc<Config>,
    client: SocketAddr,
    kind: &'static str,
    rewrite: Option<Motd>,
) -> Outcome {
    let started = Instant::now();

    let address = match mc_forwarding::resolve(&backend.address).await {
        Ok(address) => address,
        Err(err) => {
            observe::backend_failed(&backend.name, err.kind());
            warn!(backend = %backend.name, %err, "cannot resolve backend");
            let _ = kick(&mut stream, &config.messages.backend_error).await;
            return Outcome::Refused("backend_unresolved");
        }
    };

    let mut upstream =
        match mc_forwarding::connect(address, Origin::Client(client), config.timeouts.connect).await
        {
            Ok(upstream) => upstream,
            Err(err) => {
                observe::backend_failed(&backend.name, err.kind());
                warn!(backend = %backend.name, %address, error = %err, "backend connect failed");
                let _ = kick(&mut stream, &config.messages.backend_error).await;
                return Outcome::Refused("backend_error");
            }
        };

    observe::backend_connected(&backend.name);
    observe::session_started(&backend.name);

    // Everything read so far is replayed byte for byte, so the backend sees
    // exactly what the client sent — including any modloader marker in the
    // handshake address.
    if !replay.is_empty()
        && let Err(err) = upstream.write_all(&replay).await
    {
        observe::backend_failed(&backend.name, "io");
        warn!(backend = %backend.name, %err, "replaying the handshake failed");
        let _ = kick(&mut stream, &config.messages.backend_error).await;
        observe::session_ended(&backend.name, None);
        return Outcome::Refused("backend_error");
    }

    if let Some(motd) = rewrite
        && let Err(reason) =
            rewrite_status_response(&mut stream, &mut upstream, &motd, config.timeouts.status).await
    {
        debug!(backend = %backend.name, reason, "status rewrite failed");
        observe::session_ended(&backend.name, None);
        return Outcome::Dropped(reason);
    }

    let idle = config.timeouts.idle;
    let idle = (!idle.is_zero()).then_some(idle);
    // The replayed handshake went to the backend before the pipe started, so
    // the pipe's own count does not include it.
    let replayed = replay.len() as u64;
    let bytes = pipe::run(stream, upstream, idle)
        .await
        .ok()
        .map(|(to_backend, to_client)| (to_backend + replayed, to_client));

    Outcome::Proxied {
        backend: backend.name.clone(),
        kind,
        seconds: started.elapsed().as_secs(),
        bytes,
    }
}

/// Reads the backend's status response, swaps the configured MOTD lines and
/// forwards it.
///
/// A response that cannot be parsed is passed through unchanged: the client may
/// well understand something this gateway does not.
async fn rewrite_status_response(
    client: &mut TcpStream,
    upstream: &mut TcpStream,
    motd: &Motd,
    deadline: Duration,
) -> Result<(), &'static str> {
    let mut buf = Vec::with_capacity(4096);

    // Everything needed from the frame is copied out here, so the borrow of
    // `buf` ends before the buffer is reused below.
    let (packet_id, consumed, json) = {
        let frame = read_frame(upstream, &mut buf, deadline, MAX_STATUS_RESPONSE)
            .await
            .map_err(|_| "no status response from the backend")?;
        let json = frame.reader().string(MAX_STATUS_RESPONSE).ok();
        (frame.id, frame.total_len, json)
    };

    if packet_id != status::STATUS_RESPONSE_ID {
        return Err("unexpected packet from the backend");
    }

    let replacement = json
        .and_then(|json| motd::rewrite_status(&json, motd))
        .map(|json| status::encode_status_response(&json));

    match replacement {
        Some(packet) => {
            observe::motd_rewritten();
            client.write_all(&packet).await.map_err(|_| "status write failed")?;
        }
        None => client
            .write_all(&buf[..consumed])
            .await
            .map_err(|_| "status write failed")?,
    }

    // Anything the backend already sent past the response belongs to the
    // client too.
    let trailing = buf.split_off(consumed);
    if !trailing.is_empty() {
        client.write_all(&trailing).await.map_err(|_| "status write failed")?;
    }
    client.flush().await.map_err(|_| "status flush failed")
}

// ------------------------------------------------------------------ input --

enum HandshakeOutcome {
    /// The handshake, and how many bytes of `buf` it occupies.
    ///
    /// The bytes stay in the buffer: a proxied connection has to replay the
    /// handshake to the backend verbatim, so consuming it here would mean
    /// forwarding a session that never introduced itself.
    Modern(Handshake, usize),
    /// A pre-1.7 ping. The variant carries no payload because the gateway does
    /// not answer these itself — it forwards them.
    Legacy,
}

/// Reads until a complete handshake is available, leaving it in `buf`.
async fn read_handshake(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    config: &Arc<Config>,
) -> Result<HandshakeOutcome, Outcome> {
    let deadline = config.timeouts.handshake;
    let max_bytes = config.limits.max_handshake_bytes;

    loop {
        if legacy::detect(buf).is_some() {
            return Ok(HandshakeOutcome::Legacy);
        }

        match decode_frame(buf) {
            Ok(frame) => {
                let handshake = Handshake::decode(&frame).map_err(|err| {
                    // A parse failure here is either a port scanner or a client
                    // speaking something that is not Minecraft.
                    observe::handshake_error(err.kind().0);
                    Outcome::Dropped(match err {
                        ProtoError::UnknownNextState(_) => "unknown next_state",
                        _ => "malformed handshake",
                    })
                })?;
                return Ok(HandshakeOutcome::Modern(handshake, frame.total_len));
            }
            Err(err) if err.is_incomplete() => {}
            Err(err) => {
                observe::handshake_error(err.kind().0);
                return Err(Outcome::Dropped("malformed handshake"));
            }
        }

        if buf.len() >= max_bytes {
            return Err(Outcome::Refused("handshake_too_large"));
        }
        if !read_more(stream, buf, deadline).await {
            return Err(Outcome::Dropped("closed before handshake"));
        }
    }
}

/// Reads one frame, topping the buffer up as needed. Leaves it in `buf`.
async fn read_frame<'a>(
    stream: &mut TcpStream,
    buf: &'a mut Vec<u8>,
    deadline: Duration,
    max_size: usize,
) -> Result<Frame<'a>, &'static str> {
    loop {
        match decode_frame_limited(buf, max_size) {
            Ok(_) => break,
            Err(err) if err.is_incomplete() => {}
            Err(_) => return Err("malformed packet"),
        }
        if !read_more(stream, buf, deadline).await {
            return Err("connection closed");
        }
    }
    // Re-decoded after the loop so the borrow of `buf` starts here.
    decode_frame_limited(buf, max_size).map_err(|_| "malformed packet")
}

/// Appends one read to `buf`. Returns false on EOF, timeout or error.
async fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>, deadline: Duration) -> bool {
    let mut chunk = [0u8; 4096];
    match timeout(deadline, stream.read(&mut chunk)).await {
        Ok(Ok(0)) | Ok(Err(_)) | Err(_) => false,
        Ok(Ok(n)) => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
    }
}

/// Reads an inbound PROXY header when the peer is a trusted upstream hop.
///
/// The trust check is the whole point: a header from anywhere else is ignored,
/// so no client can talk its way into a different source IP — which matters all
/// the more on Linux, where that address is what the backend will see.
async fn read_inbound_header(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    settings: &InboundProxyProtocol,
    peer: SocketAddr,
    deadline: Duration,
) -> Result<SocketAddr, &'static str> {
    if !settings.enabled || !settings.trusted.contains(peer.ip()) {
        return Ok(peer);
    }

    loop {
        match inbound::parse(buf) {
            // A LOCAL header means the upstream is health-checking us.
            Ok((source, len)) => {
                buf.drain(..len);
                return Ok(source.unwrap_or(peer));
            }
            Err(err) if err.is_incomplete() => {}
            Err(inbound::Error::NotPresent) if !settings.required => return Ok(peer),
            Err(_) => return Err("proxy_protocol"),
        }
        if !read_more(stream, buf, deadline).await {
            return Err("proxy_protocol");
        }
    }
}

/// Sends a login disconnect so the player sees a reason instead of a timeout.
async fn kick(stream: &mut TcpStream, message: &str) -> io::Result<()> {
    stream.write_all(&login::encode_disconnect(&chat::component(message))).await?;
    stream.flush().await
}
