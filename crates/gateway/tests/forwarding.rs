//! Client IP forwarding and the trust boundary around it.
//!
//! On Linux the backend connection is transparent, so these tests assert the
//! platform-independent half: nothing is ever prepended to the stream, and no
//! client can talk the gateway into a different source address.

mod common;

use common::{Behaviour, FakeBackend, gateway, login};
use mc_protocol::{Handshake, NextState, frame::decode_frame};
use proxy_header::{ProxiedAddress, ProxyHeader};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(backend: &str, listener_extra: &str) -> String {
    format!(
        r#"
listeners:
  - name: public
    bind: "127.0.0.1:0"
{listener_extra}
routing:
  default: survival
servers:
  survival:
    address: "{backend}"
timeouts:
  drain: 200ms
health:
  enabled: false
"#
    )
}

fn v2_header(source: &str, destination: std::net::SocketAddr) -> Vec<u8> {
    let header = ProxyHeader::with_address(ProxiedAddress::stream(
        source.parse().unwrap(),
        destination,
    ));
    let mut buf = vec![0u8; 128];
    let len = header.encode_to_slice_v2(&mut buf).expect("encodes");
    buf.truncate(len);
    buf
}

#[tokio::test]
async fn the_backend_receives_minecraft_bytes_and_nothing_else() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string(), "")).await;
    let address = server.address("public").unwrap();

    let _client = login(address, "survival.example.net", "Alice").await;
    let seen = backend.wait_for_payload_connections(1).await;

    // No PROXY header, no framing of our own: the first byte is the handshake's
    // length prefix. This is what lets vanilla and every modloader work.
    let frame = decode_frame(&seen[0].head).expect("a handshake, first thing");
    let handshake = Handshake::decode(&frame).expect("valid handshake");
    assert_eq!(handshake.server_address, "survival.example.net");
    assert_eq!(handshake.next_state, NextState::Login);

    server.shutdown().await;
}

#[tokio::test]
async fn a_trusted_inbound_header_sets_the_client_address() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let listener_extra =
        "    proxy_protocol:\n      enabled: true\n      trusted: [\"127.0.0.0/8\"]";
    let server = gateway(&config(&backend.address.to_string(), listener_extra)).await;
    let address = server.address("public").unwrap();

    // Pretend to be an upstream load balancer announcing the real client.
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut request = v2_header("203.0.113.7:51234", address);
    request.extend_from_slice(&common::handshake(
        767,
        "survival.example.net",
        25565,
        NextState::Login,
    ));
    request.extend_from_slice(&common::login_start("Alice"));
    stream.write_all(&request).await.unwrap();

    let seen = backend.wait_for_payload_connections(1).await;

    // The header is consumed, not forwarded: the backend gets the handshake.
    let frame = decode_frame(&seen[0].head).expect("a handshake, first thing");
    assert_eq!(Handshake::decode(&frame).unwrap().hostname(), "survival.example.net");

    server.shutdown().await;
}

#[tokio::test]
async fn an_untrusted_inbound_header_is_never_believed() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    // 127.0.0.1 is deliberately *not* in the trust list.
    let listener_extra =
        "    proxy_protocol:\n      enabled: true\n      trusted: [\"10.0.0.0/8\"]";
    let server = gateway(&config(&backend.address.to_string(), listener_extra)).await;
    let address = server.address("public").unwrap();

    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut request = v2_header("198.51.100.66:1234", address);
    request.extend_from_slice(&common::handshake(
        767,
        "survival.example.net",
        25565,
        NextState::Login,
    ));
    stream.write_all(&request).await.unwrap();

    // The header is treated as ordinary bytes, which cannot parse as a
    // handshake, so the connection is dropped rather than trusted.
    let mut response = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.read_to_end(&mut response),
    )
    .await
    .expect("the gateway closed the connection");

    assert!(response.is_empty(), "no data should come back");
    assert_eq!(backend.connection_count(), 0, "nothing was forwarded");

    server.shutdown().await;
}

#[tokio::test]
async fn a_missing_header_from_a_trusted_peer_is_allowed_unless_required() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let listener_extra =
        "    proxy_protocol:\n      enabled: true\n      trusted: [\"127.0.0.0/8\"]";
    let server = gateway(&config(&backend.address.to_string(), listener_extra)).await;
    let address = server.address("public").unwrap();

    // A direct client on a listener that merely *accepts* headers still works.
    let _client = login(address, "survival.example.net", "Alice").await;
    backend.wait_for_payload_connections(1).await;

    server.shutdown().await;
}

#[tokio::test]
async fn a_required_header_is_enforced() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let listener_extra = "    proxy_protocol:\n      enabled: true\n      required: true\n      trusted: [\"127.0.0.0/8\"]";
    let server = gateway(&config(&backend.address.to_string(), listener_extra)).await;
    let address = server.address("public").unwrap();

    let mut client = login(address, "survival.example.net", "Alice").await;
    let mut response = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        client.read_to_end(&mut response),
    )
    .await
    .expect("the gateway closed the connection");
    assert_eq!(backend.connection_count(), 0);

    server.shutdown().await;
}
