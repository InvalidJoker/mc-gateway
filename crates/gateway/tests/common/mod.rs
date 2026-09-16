//! Harness for the integration tests: a real gateway, fake backends and a
//! hand-rolled Minecraft client that speaks the wire format directly.

#![allow(dead_code, reason = "shared harness; not every test uses every helper")]

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use mc_config::Config;
use mc_gateway::server::Server;
use mc_protocol::{
    Reader, Writer, encode_packet,
    frame::decode_frame_limited,
    handshake::{HANDSHAKE_ID, NextState},
    login::LOGIN_START_ID,
    status::{PING_REQUEST_ID, STATUS_REQUEST_ID},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};

/// What a fake backend does with a connection.
#[derive(Debug, Clone)]
pub enum Behaviour {
    /// Record what arrives, then echo everything back.
    Echo,
    /// Answer a status request with this JSON document.
    Status(String),
    /// Accept and stay silent.
    Silent,
}

/// One connection a fake backend saw.
#[derive(Debug, Clone)]
pub struct Received {
    pub peer: SocketAddr,
    /// Bytes from the first read, which is where the PROXY header and the
    /// replayed handshake land.
    pub head: Vec<u8>,
}

/// A stand-in Minecraft server.
pub struct FakeBackend {
    pub address: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
}

impl FakeBackend {
    pub async fn start(behaviour: Behaviour) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake backend");
        let address = listener.local_addr().expect("local addr");
        let received = Arc::new(Mutex::new(Vec::new()));

        let log = Arc::clone(&received);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, peer)) = listener.accept().await else { return };
                let log = Arc::clone(&log);
                let behaviour = behaviour.clone();
                tokio::spawn(async move {
                    let mut head = vec![0u8; 8192];
                    let Ok(read) = stream.read(&mut head).await else { return };
                    head.truncate(read);
                    log.lock().expect("backend log").push(Received { peer, head });

                    match behaviour {
                        Behaviour::Echo => {
                            let mut buf = vec![0u8; 8192];
                            loop {
                                match stream.read(&mut buf).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => {
                                        if stream.write_all(&buf[..n]).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        Behaviour::Status(json) => {
                            let _ = stream
                                .write_all(&mc_protocol::status::encode_status_response(&json))
                                .await;
                            let _ = stream.flush().await;
                            time::sleep(Duration::from_millis(200)).await;
                        }
                        Behaviour::Silent => {
                            std::future::pending::<()>().await;
                        }
                    }
                });
            }
        });

        Self { address, received }
    }

    pub fn connections(&self) -> Vec<Received> {
        self.received.lock().expect("backend log").clone()
    }

    pub fn connection_count(&self) -> usize {
        self.received.lock().expect("backend log").len()
    }

    /// Connections that actually sent something.
    ///
    /// TCP health checks connect and close without a byte, so a test that
    /// counts sessions has to ignore them.
    pub fn payload_connections(&self) -> Vec<Received> {
        self.connections().into_iter().filter(|c| !c.head.is_empty()).collect()
    }

    pub async fn wait_for_payload_connections(&self, count: usize) -> Vec<Received> {
        for _ in 0..200 {
            if self.payload_connections().len() >= count {
                return self.payload_connections();
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {count} sessions, saw {}",
            self.payload_connections().len()
        );
    }

    /// Waits for at least `count` connections, or panics with what it saw.
    pub async fn wait_for_connections(&self, count: usize) -> Vec<Received> {
        for _ in 0..200 {
            if self.connection_count() >= count {
                return self.connections();
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {count} connections, saw {}", self.connection_count());
    }
}

/// Starts a gateway from a YAML string. Listeners must bind to port 0.
pub async fn gateway(yaml: &str) -> Server {
    let loaded = Config::parse(yaml, "test").expect("config parses");
    Server::start(loaded, std::path::PathBuf::from("test-config.yaml"))
        .await
        .expect("gateway starts")
}

/// Starts a gateway from a file so the test can rewrite it and reload.
pub async fn gateway_from_file(path: &std::path::Path) -> Server {
    let loaded = Config::load(path).expect("config loads");
    Server::start(loaded, path.to_path_buf()).await.expect("gateway starts")
}

// ------------------------------------------------------------------ client --

pub fn handshake(protocol: i32, host: &str, port: u16, next: NextState) -> Vec<u8> {
    let mut body = Writer::new();
    body.varint(protocol).string(host).u16(port).varint(next.as_i32());
    encode_packet(HANDSHAKE_ID, body.as_slice())
}

pub fn login_start(name: &str) -> Vec<u8> {
    let mut body = Writer::new();
    body.string(name).bytes(&[0u8; 16]);
    encode_packet(LOGIN_START_ID, body.as_slice())
}

/// Performs a full server list ping and returns the parsed JSON.
pub async fn status_ping(gateway: SocketAddr, host: &str, protocol: i32) -> serde_json::Value {
    let mut stream = TcpStream::connect(gateway).await.expect("connect to gateway");
    let mut request = handshake(protocol, host, 25565, NextState::Status);
    request.extend_from_slice(&encode_packet(STATUS_REQUEST_ID, &[]));
    stream.write_all(&request).await.expect("send status request");

    let frame = read_packet(&mut stream).await.expect("status response");
    let json = Reader::new(&frame.1).string(262_144).expect("status json");
    serde_json::from_str(&json).expect("valid status json")
}

/// Status ping plus the latency ping, returning the pong payload.
pub async fn status_ping_with_pong(gateway: SocketAddr, host: &str) -> (serde_json::Value, i64) {
    let mut stream = TcpStream::connect(gateway).await.expect("connect to gateway");
    let mut request = handshake(767, host, 25565, NextState::Status);
    request.extend_from_slice(&encode_packet(STATUS_REQUEST_ID, &[]));
    stream.write_all(&request).await.expect("send status request");

    let (_, body) = read_packet(&mut stream).await.expect("status response");
    let json: serde_json::Value =
        serde_json::from_str(&Reader::new(&body).string(262_144).expect("json")).expect("json");

    let mut ping = Writer::new();
    ping.i64(0x1234_5678);
    stream
        .write_all(&encode_packet(PING_REQUEST_ID, ping.as_slice()))
        .await
        .expect("send ping");

    let (id, body) = read_packet(&mut stream).await.expect("pong");
    assert_eq!(id, PING_REQUEST_ID);
    (json, Reader::new(&body).i64().expect("pong payload"))
}

/// Connects and sends handshake + login start, leaving the stream open.
pub async fn login(gateway: SocketAddr, host: &str, name: &str) -> TcpStream {
    login_with_protocol(gateway, host, name, 767).await
}

pub async fn login_with_protocol(
    gateway: SocketAddr,
    host: &str,
    name: &str,
    protocol: i32,
) -> TcpStream {
    let mut stream = TcpStream::connect(gateway).await.expect("connect to gateway");
    let mut request = handshake(protocol, host, 25565, NextState::Login);
    request.extend_from_slice(&login_start(name));
    stream.write_all(&request).await.expect("send login");
    stream
}

/// Reads a login disconnect and returns its plain text.
pub async fn read_kick(stream: &mut TcpStream) -> String {
    let (id, body) = read_packet(stream).await.expect("disconnect packet");
    assert_eq!(id, 0x00, "login disconnect has packet id 0");
    let json = Reader::new(&body).string(262_144).expect("disconnect json");
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid chat json");
    mc_protocol::chat::component_to_plain(&value)
}

/// Reads one length-prefixed packet, returning `(id, body)`.
pub async fn read_packet(stream: &mut TcpStream) -> Option<(i32, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Ok(frame) = decode_frame_limited(&buf, 1024 * 1024) {
            return Some((frame.id, frame.body.to_vec()));
        }
        let read = time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if read == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

/// Polls `check` until it is true, or panics after `label` times out.
pub async fn wait_until(label: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..300 {
        if check() {
            return;
        }
        time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {label}");
}

/// Reads whatever arrives within a short window.
pub async fn read_some(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = vec![0u8; 4096];
    match time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await {
        Ok(Ok(n)) => buf[..n].to_vec(),
        _ => Vec::new(),
    }
}
