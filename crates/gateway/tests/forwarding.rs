//! Client IP forwarding, and the trust boundary around it.

mod common;

use common::{Behaviour, FakeBackend, gateway, login};
use mc_forwarding::proxy_protocol::{self, ProxyHeader};
use mc_protocol::{Handshake, NextState, frame::decode_frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(backend: &str, forwarding: &str, listener_extra: &str) -> String {
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
    forwarding: {forwarding}
timeouts:
  drain: 200ms
health:
  enabled: false
"#
    )
}

#[tokio::test]
async fn proxy_protocol_carries_the_real_client_address() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string(), "proxy_protocol_v2", "")).await;
    let address = server.address("public").unwrap();

    let client = login(address, "survival.example.net", "Alice").await;
    let client_port = client.local_addr().unwrap().port();
    let seen = backend.wait_for_connections(1).await;

    let (header, len) = proxy_protocol::parse(&seen[0].head).expect("a PROXY header");
    let ProxyHeader::Proxy { src, dst } = header else { panic!("expected a PROXY command") };
    assert_eq!(src.ip().to_string(), "127.0.0.1");
    assert_eq!(src.port(), client_port, "the client's own source port, not the gateway's");
    assert_eq!(dst.port(), address.port(), "destination is the address the client dialled");

    // The handshake follows the header, untouched.
    let frame = decode_frame(&seen[0].head[len..]).expect("handshake after the header");
    assert_eq!(Handshake::decode(&frame).unwrap().hostname(), "survival.example.net");

    server.shutdown().await;
}

#[tokio::test]
async fn plain_forwarding_sends_no_header_at_all() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string(), "none", "")).await;
    let address = server.address("public").unwrap();

    let _client = login(address, "survival.example.net", "Alice").await;
    let seen = backend.wait_for_connections(1).await;

    // A vanilla backend must receive Minecraft bytes and nothing else.
    assert!(proxy_protocol::parse(&seen[0].head).is_err());
    let frame = decode_frame(&seen[0].head).expect("handshake first");
    assert_eq!(Handshake::decode(&frame).unwrap().next_state, NextState::Login);

    server.shutdown().await;
}

#[tokio::test]
async fn a_trusted_inbound_header_is_passed_through_to_the_backend() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let listener_extra = "    proxy_protocol:\n      enabled: true\n      trusted: [\"127.0.0.0/8\"]";
    let server =
        gateway(&config(&backend.address.to_string(), "proxy_protocol_v2", listener_extra)).await;
    let address = server.address("public").unwrap();

    // Pretend to be an upstream load balancer announcing the real client.
    let real_client = "203.0.113.7:51234".parse().unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut request = proxy_protocol::encode_v2(real_client, address);
    request.extend_from_slice(&common::handshake(767, "survival.example.net", 25565, NextState::Login));
    request.extend_from_slice(&common::login_start("Alice"));
    stream.write_all(&request).await.unwrap();

    let seen = backend.wait_for_connections(1).await;
    let (header, _) = proxy_protocol::parse(&seen[0].head).expect("a PROXY header");
    assert_eq!(
        header.source(),
        Some(real_client),
        "the client IP from the trusted hop must survive the second hop"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn an_untrusted_inbound_header_is_never_believed() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    // 127.0.0.1 is deliberately *not* in the trust list.
    let listener_extra = "    proxy_protocol:\n      enabled: true\n      trusted: [\"10.0.0.0/8\"]";
    let server =
        gateway(&config(&backend.address.to_string(), "proxy_protocol_v2", listener_extra)).await;
    let address = server.address("public").unwrap();

    let spoofed = "198.51.100.66:1234".parse().unwrap();
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut request = proxy_protocol::encode_v2(spoofed, address);
    request.extend_from_slice(&common::handshake(767, "survival.example.net", 25565, NextState::Login));
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
async fn a_status_ping_proxied_upstream_uses_a_local_header() {
    let json = r#"{"players":{"max":1,"online":0}}"#;
    let backend = FakeBackend::start(Behaviour::Status(json.to_owned())).await;
    let yaml = format!(
        r#"
listeners:
  - name: public
    bind: "127.0.0.1:0"
routing:
  default: survival
servers:
  survival:
    address: "{}"
    forwarding: proxy_protocol_v2
motd:
  enabled: false
timeouts:
  drain: 200ms
health:
  enabled: false
"#,
        backend.address
    );
    let server = gateway(&yaml).await;
    let address = server.address("public").unwrap();

    let _status = common::status_ping(address, "survival.example.net", 767).await;
    let seen = backend.wait_for_connections(1).await;

    // A ping is not a player, so the gateway must not assert one.
    let (header, _) = proxy_protocol::parse(&seen[0].head).expect("a PROXY header");
    assert_eq!(header, ProxyHeader::Local);

    server.shutdown().await;
}
