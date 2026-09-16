//! End to end: a real gateway, real sockets, a client that speaks the wire
//! format, and fake backends that record exactly what arrived.

mod common;

use common::{Behaviour, FakeBackend, gateway, login, read_kick, read_some};
use mc_protocol::{Handshake, frame::decode_frame};
use tokio::io::AsyncWriteExt;

fn config(survival: &str, modded: &str) -> String {
    format!(
        r#"
listeners:
  - name: public
    bind: "127.0.0.1:0"
routing:
  default: survival
  rules:
    - host: "survival.example.net"
      target: survival
    - host: "modded.example.net"
      target: modded
    - host: "*.play.example.net"
      target: survival
servers:
  survival:
    address: "{survival}"
  modded:
    address: "{modded}"
health:
  enabled: false
timeouts:
  drain: 200ms
"#
    )
}

#[tokio::test]
async fn routes_by_handshake_hostname() {
    let survival = FakeBackend::start(Behaviour::Echo).await;
    let modded = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&survival.address.to_string(), &modded.address.to_string())).await;
    let address = server.address("public").unwrap();

    let _a = login(address, "survival.example.net", "Alice").await;
    let _b = login(address, "modded.example.net", "Bob").await;

    survival.wait_for_connections(1).await;
    modded.wait_for_connections(1).await;
    assert_eq!(survival.connection_count(), 1);
    assert_eq!(modded.connection_count(), 1);

    server.shutdown().await;
}

#[tokio::test]
async fn the_handshake_reaches_the_backend_byte_for_byte() {
    let survival = FakeBackend::start(Behaviour::Echo).await;
    let modded = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&survival.address.to_string(), &modded.address.to_string())).await;
    let address = server.address("public").unwrap();

    let _client = login(address, "survival.example.net", "Alice").await;
    let seen = survival.wait_for_connections(1).await;

    // The backend must see the original handshake, not a rewritten one: that is
    // what keeps modloaders and version detection working.
    let frame = decode_frame(&seen[0].head).expect("handshake frame");
    let handshake = Handshake::decode(&frame).expect("valid handshake");
    assert_eq!(handshake.server_address, "survival.example.net");
    assert_eq!(handshake.protocol_version, 767);
    assert_eq!(handshake.next_state, mc_protocol::NextState::Login);

    // The login start packet that followed in the same segment is there too.
    let login_frame = decode_frame(&seen[0].head[frame.total_len..]).expect("login start");
    assert_eq!(login_frame.id, 0x00);

    server.shutdown().await;
}

#[tokio::test]
async fn a_forge_marker_routes_on_the_plain_hostname_and_is_forwarded_intact() {
    let survival = FakeBackend::start(Behaviour::Echo).await;
    let modded = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&survival.address.to_string(), &modded.address.to_string())).await;
    let address = server.address("public").unwrap();

    let _client = login(address, "modded.example.net\0FML2\0", "Bob").await;
    let seen = modded.wait_for_connections(1).await;

    let frame = decode_frame(&seen[0].head).expect("handshake frame");
    let handshake = Handshake::decode(&frame).expect("valid handshake");
    assert_eq!(handshake.hostname(), "modded.example.net", "routed on the bare host");
    assert_eq!(
        handshake.server_address, "modded.example.net\0FML2\0",
        "the marker must survive: the backend needs it to recognise a modded client"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn wildcards_and_the_default_target_both_apply() {
    let survival = FakeBackend::start(Behaviour::Echo).await;
    let modded = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&survival.address.to_string(), &modded.address.to_string())).await;
    let address = server.address("public").unwrap();

    let _wildcard = login(address, "eu.play.example.net", "Alice").await;
    let _fallback = login(address, "who.knows.invalid", "Bob").await;

    survival.wait_for_connections(2).await;
    assert_eq!(modded.connection_count(), 0);

    server.shutdown().await;
}

#[tokio::test]
async fn traffic_flows_in_both_directions_after_routing() {
    let survival = FakeBackend::start(Behaviour::Echo).await;
    let modded = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&survival.address.to_string(), &modded.address.to_string())).await;
    let address = server.address("public").unwrap();

    let mut client = login(address, "survival.example.net", "Alice").await;
    survival.wait_for_connections(1).await;

    // The echo backend sends back everything after the first read, so this
    // proves the pipe is live in both directions.
    client.write_all(b"post-login payload").await.unwrap();
    let echoed = read_some(&mut client).await;
    assert_eq!(echoed, b"post-login payload");

    server.shutdown().await;
}

#[tokio::test]
async fn an_unknown_host_is_refused_with_a_reason() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let yaml = format!(
        r#"
listeners:
  - name: public
    bind: "127.0.0.1:0"
routing:
  rules:
    - host: "survival.example.net"
      target: survival
servers:
  survival:
    address: "{}"
messages:
  no_route: "&cUnknown server address"
health:
  enabled: false
timeouts:
  drain: 200ms
"#,
        backend.address
    );
    let server = gateway(&yaml).await;
    let address = server.address("public").unwrap();

    let mut client = login(address, "typo.example.net", "Alice").await;
    assert_eq!(read_kick(&mut client).await, "Unknown server address");
    assert_eq!(backend.connection_count(), 0, "nothing reached a backend");

    server.shutdown().await;
}

#[tokio::test]
async fn a_dead_backend_produces_a_kick_not_a_hang() {
    // Bind and drop: a closed port that is still syntactically valid.
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let yaml = format!(
        r#"
listeners:
  - name: public
    bind: "127.0.0.1:0"
routing:
  default: dead
servers:
  dead:
    address: "{dead}"
messages:
  backend_error: "&cCould not reach the server"
timeouts:
  connect: 500ms
  drain: 200ms
health:
  enabled: false
"#
    );
    let server = gateway(&yaml).await;
    let address = server.address("public").unwrap();

    let mut client = login(address, "anything.example.net", "Alice").await;
    assert_eq!(read_kick(&mut client).await, "Could not reach the server");

    server.shutdown().await;
}

#[tokio::test]
async fn an_unhealthy_target_is_reported_as_offline() {
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let yaml = format!(
        r#"
listeners:
  - name: public
    bind: "127.0.0.1:0"
routing:
  default: dead
servers:
  dead:
    address: "{dead}"
messages:
  backend_offline: "&cThis server is currently offline"
timeouts:
  drain: 200ms
health:
  method: tcp
  interval: 30ms
  timeout: 20ms
  fall: 1
  rise: 1
"#
    );
    let server = gateway(&yaml).await;
    let address = server.address("public").unwrap();
    let registry = server.app.runtime().registry.clone();

    common::wait_until("the backend to be marked down", || {
        !registry.backend("dead").unwrap().is_healthy()
    })
    .await;

    let mut client = login(address, "anything.example.net", "Alice").await;
    assert_eq!(
        read_kick(&mut client).await,
        "This server is currently offline",
        "health checks short-circuit the connect attempt"
    );

    server.shutdown().await;
}
