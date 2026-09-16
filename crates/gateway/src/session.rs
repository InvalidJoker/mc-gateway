//! One client connection, from `accept()` to close.
//!
//! The state machine is deliberately short: read the handshake, decide, and
//! then either answer a status ping locally or become a byte pipe. Everything
//! the gateway inspects happens in the first few hundred bytes.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use mc_config::{Config, Forwarding, InboundProxyProtocol, Motd};
use mc_forwarding::{Origin, proxy_protocol};
use mc_protocol::{
    Error as ProtoError, Handshake, NextState, chat, frame::decode_frame, legacy, login, status,
};
use mc_routing::{Registry, SelectError, SessionGuard};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Instant, timeout},
};
use tracing::{debug, info, warn};

use crate::{
    app::{App, ListenerRuntime},
    limits::Rejection,
    motd::{self, MotdContext},
    pipe,
};

/// How a connection ended, for logs and metrics.
#[derive(Debug)]
enum Outcome {
    /// Answered a status ping ourselves.
    Status,
    /// Answered a pre-1.7 ping.
    LegacyStatus,
    /// Proxied a session.
    Proxied { backend: String, seconds: u64, bytes: pipe::Transferred },
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
    app.metrics.connection_opened();
    let started = Instant::now();

    let outcome = run(stream, peer, &listener, &app).await;
    app.metrics.connection_closed();

    let client = app.log_addr(peer);
    match outcome {
        Outcome::Proxied { backend, seconds, bytes } => {
            app.metrics.session_finished(seconds);
            info!(
                listener = %listener.name,
                client = %client,
                %backend,
                seconds,
                to_backend = bytes.to_backend,
                to_client = bytes.to_client,
                "session ended"
            );
        }
        Outcome::Status | Outcome::LegacyStatus => {
            debug!(listener = %listener.name, client = %client, "status ping answered");
        }
        Outcome::Refused(reason) => {
            app.metrics.connections_rejected.increment(reason);
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

    let local = stream.local_addr().unwrap_or(listener.bind);
    let runtime = app.runtime();
    let config = runtime.registry.config();
    let mut buf = Vec::with_capacity(512);

    // ---- 1. inbound PROXY protocol, if an upstream L4 hop is trusted -------
    let client = match read_inbound_header(
        &mut stream,
        &mut buf,
        &listener.proxy_protocol,
        peer,
        config.timeouts.handshake.get(),
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
        Ok(HandshakeOutcome::Legacy(kind)) => {
            app.metrics.status_requests.increment("legacy");
            let motd = config.motd.overlay(None);
            let _ = answer_legacy(&mut stream, kind, &motd, &runtime.registry).await;
            return Outcome::LegacyStatus;
        }
        Err(reason) => return reason,
    };

    let host = handshake.hostname();
    let route = runtime.registry.route(&host, &listener.name);
    let motd = config.motd.overlay(route.as_ref().and_then(|r| r.rule).and_then(|r| r.motd.as_ref()));

    match handshake.next_state {
        NextState::Status => {
            serve_status(
                stream,
                buf,
                handshake_len,
                &handshake,
                route,
                &motd,
                app,
                &runtime.registry,
            )
            .await
        }
        NextState::Login | NextState::Transfer => {
            serve_login(stream, buf, handshake_len, &handshake, route, client, local, app, config)
                .await
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
    route: Option<mc_routing::Route<'_>>,
    motd: &Motd,
    app: &Arc<App>,
    registry: &Arc<Registry>,
) -> Outcome {
    let config = registry.config();

    // A route the gateway does not serve, or one whose backends are all down,
    // still gets an answer — just a truthful one.
    let offline = match &route {
        Some(route) => route.target.healthy_members().next().is_none(),
        None => true,
    };

    if !motd.enabled {
        // Proxying the ping means the server list follows the backend's own
        // MOTD, at the price of going dark whenever the backend does.
        if let Some(route) = &route
            && let Ok(backend) = route.target.select()
        {
            app.metrics.status_requests.increment("backend");
            // The backend has to see the handshake too, so the whole buffer is
            // replayed untouched.
            return proxy_to_backend(stream, buf, &backend, app, config, None).await;
        }
    }

    app.metrics.status_requests.increment("gateway");
    // Answering here, so the handshake has served its purpose.
    buf.drain(..handshake_len);

    let runtime = app.runtime();
    let response = motd::build(
        motd,
        MotdContext {
            client_protocol: handshake.protocol_version,
            sessions: registry.active_sessions() as i64,
            reported: registry.reported_players(),
            favicon: runtime.favicon.as_deref(),
            offline,
        },
    );

    let deadline = config.timeouts.status.get();
    match exchange_status(&mut stream, &mut buf, &response.to_json(), deadline).await {
        Ok(()) => Outcome::Status,
        Err(reason) => Outcome::Dropped(reason),
    }
}

/// Status request -> response, then the optional ping -> pong.
async fn exchange_status(
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
    let ping = match read_frame(stream, buf, deadline, mc_protocol::MAX_PACKET_SIZE).await {
        Ok(frame) => frame,
        Err(_) => return Ok(()),
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

async fn answer_legacy(
    stream: &mut TcpStream,
    kind: legacy::LegacyPing,
    motd: &Motd,
    registry: &Arc<Registry>,
) -> std::io::Result<()> {
    let text = chat::component_to_plain(&chat::component(&motd.text));
    let response = legacy::encode_response(
        kind,
        // 1.6 and older cannot speak to a modern server anyway; what matters is
        // that the entry renders with a MOTD instead of an error.
        127,
        &motd.version_name,
        &text,
        registry.active_sessions() as i64,
        motd.max_players,
    );
    stream.write_all(&response).await?;
    stream.flush().await
}

// ------------------------------------------------------------------ login --

#[allow(clippy::too_many_arguments, reason = "one connection's full context")]
async fn serve_login(
    mut stream: TcpStream,
    buf: Vec<u8>,
    handshake_len: usize,
    handshake: &Handshake,
    route: Option<mc_routing::Route<'_>>,
    client: SocketAddr,
    local: SocketAddr,
    app: &Arc<App>,
    config: &Arc<Config>,
) -> Outcome {
    app.metrics.login_attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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
        forwarding = %backend.forwarding,
        "routing player"
    );

    proxy_to_backend(
        stream,
        buf,
        &backend,
        app,
        config,
        Some(LoginContext { client, local, guard }),
    )
    .await
}

/// Extra state a login carries that a proxied status ping does not.
struct LoginContext {
    client: SocketAddr,
    local: SocketAddr,
    /// Holds the backend's session slot until the session ends. Never read:
    /// its only job is to release the slot when this struct is dropped.
    #[allow(dead_code, reason = "RAII guard")]
    guard: SessionGuard,
}

async fn proxy_to_backend(
    mut stream: TcpStream,
    replay: Vec<u8>,
    backend: &Arc<mc_routing::Backend>,
    app: &Arc<App>,
    config: &Arc<Config>,
    login: Option<LoginContext>,
) -> Outcome {
    let started = Instant::now();

    let origin = match &login {
        Some(ctx) => Origin::Client { src: ctx.client, dst: ctx.local },
        // A proxied status ping carries no player identity, so it is opened as
        // the gateway rather than on someone's behalf.
        None => Origin::Gateway,
    };
    // A status ping must never be forwarded transparently: binding to the
    // client's address for a probe would put a stray socket on the host.
    let forwarding = match (&login, backend.forwarding) {
        (None, Forwarding::Transparent) => Forwarding::None,
        (_, other) => other,
    };

    let address = match mc_forwarding::resolve(&backend.address).await {
        Ok(address) => address,
        Err(err) => {
            app.metrics.backend_failures.increment(&backend.name);
            warn!(backend = %backend.name, %err, "cannot resolve backend");
            let _ = kick(&mut stream, &config.messages.backend_error).await;
            return Outcome::Refused("backend_unresolved");
        }
    };

    let mut upstream = match mc_forwarding::connect(
        address,
        forwarding,
        origin,
        config.timeouts.connect.get(),
    )
    .await
    {
        Ok(upstream) => upstream,
        Err(err) => {
            app.metrics.backend_failures.increment(&backend.name);
            warn!(backend = %backend.name, %address, error = %err, "backend connect failed");
            let _ = kick(&mut stream, &config.messages.backend_error).await;
            return Outcome::Refused("backend_error");
        }
    };

    app.metrics.backend_connections.increment(&backend.name);

    // Everything read so far is replayed byte for byte, so the backend sees
    // exactly what the client sent — including any modloader marker in the
    // handshake address.
    if !replay.is_empty()
        && let Err(err) = upstream.write_all(&replay).await
    {
        app.metrics.backend_failures.increment(&backend.name);
        warn!(backend = %backend.name, %err, "replaying the handshake failed");
        let _ = kick(&mut stream, &config.messages.backend_error).await;
        return Outcome::Refused("backend_error");
    }

    let idle = config.timeouts.idle.get();
    let idle = (!idle.is_zero()).then_some(idle);
    let bytes = pipe::run(stream, upstream, idle, &app.metrics).await;

    drop(login);
    Outcome::Proxied {
        backend: backend.name.clone(),
        seconds: started.elapsed().as_secs(),
        bytes,
    }
}

// ------------------------------------------------------------------ input --

enum HandshakeOutcome {
    /// The handshake, and how many bytes of `buf` it occupies.
    ///
    /// The bytes stay in the buffer: a login has to replay the handshake to the
    /// backend verbatim, so consuming it here would mean forwarding a session
    /// that never introduced itself.
    Modern(Handshake, usize),
    Legacy(legacy::LegacyPing),
}

/// Reads until a complete handshake is available, leaving it in `buf`.
async fn read_handshake(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    config: &Arc<Config>,
) -> Result<HandshakeOutcome, Outcome> {
    let deadline = config.timeouts.handshake.get();
    let max_bytes = config.limits.max_handshake_bytes;

    loop {
        if let Some(kind) = legacy::detect(buf) {
            return Ok(HandshakeOutcome::Legacy(kind));
        }

        match decode_frame(buf) {
            Ok(frame) => {
                let handshake = Handshake::decode(&frame).map_err(|err| {
                    // A parse failure here is either a port scanner or a client
                    // speaking something that is not Minecraft.
                    Outcome::Dropped(match err {
                        ProtoError::UnknownNextState(_) => "unknown next_state",
                        _ => "malformed handshake",
                    })
                })?;
                return Ok(HandshakeOutcome::Modern(handshake, frame.total_len));
            }
            Err(err) if err.is_incomplete() => {}
            Err(_) => return Err(Outcome::Dropped("malformed handshake")),
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
) -> Result<mc_protocol::Frame<'a>, &'static str> {
    loop {
        match mc_protocol::frame::decode_frame_limited(buf, max_size) {
            Ok(_) => break,
            Err(err) if err.is_incomplete() => {}
            Err(_) => return Err("malformed packet"),
        }
        if !read_more(stream, buf, deadline).await {
            return Err("connection closed");
        }
    }
    // Re-decoded after the loop so the borrow of `buf` starts here.
    mc_protocol::frame::decode_frame_limited(buf, max_size).map_err(|_| "malformed packet")
}

/// Appends one read to `buf`. Returns false on EOF, timeout or error.
async fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>, deadline: Duration) -> bool {
    let mut chunk = [0u8; 1024];
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
/// so no client can talk its way into a different source IP.
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
        match proxy_protocol::parse(buf) {
            Ok((header, len)) => {
                buf.drain(..len);
                // A LOCAL header means the upstream is health-checking us.
                return Ok(header.source().unwrap_or(peer));
            }
            Err(err) if err.is_incomplete() => {}
            Err(proxy_protocol::ParseError::NotPresent) if !settings.required => {
                return Ok(peer);
            }
            Err(_) => return Err("proxy_protocol"),
        }
        if !read_more(stream, buf, deadline).await {
            return Err("proxy_protocol");
        }
    }
}

/// Sends a login disconnect so the player sees a reason instead of a timeout.
async fn kick(stream: &mut TcpStream, message: &str) -> std::io::Result<()> {
    stream.write_all(&login::encode_disconnect(&chat::component(message))).await?;
    stream.flush().await
}
