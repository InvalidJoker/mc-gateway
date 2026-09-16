//! Admission control at the edge, and the counters that make it observable.

mod common;

use std::time::Duration;

use common::{Behaviour, FakeBackend, gateway, login, read_kick};
use tokio::io::AsyncReadExt;

fn config(backend: &str, limits: &str) -> String {
    format!(
        r#"
listeners:
  - name: public
    bind: "127.0.0.1:0"
routing:
  default: survival
servers:
  survival:
    address: "{backend}"
limits:
{limits}
messages:
  rate_limited: "&cSlow down"
  too_many_connections: "&cServer full"
timeouts:
  drain: 200ms
health:
  enabled: false
"#
    )
}

#[tokio::test]
async fn too_many_connections_from_one_ip_are_dropped() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let limits = "  max_connections_per_ip: 2\n  connection_rate:\n    burst: 0\n    per: 60s";
    let server = gateway(&config(&backend.address.to_string(), limits)).await;
    let address = server.address("public").unwrap();

    let _first = login(address, "survival.example.net", "Alice").await;
    let _second = login(address, "survival.example.net", "Bob").await;
    backend.wait_for_connections(2).await;

    let mut third = login(address, "survival.example.net", "Carol").await;
    let mut response = Vec::new();
    // Either a clean EOF or a reset: the gateway closes without reading the
    // bytes this client already sent, so the kernel may answer with an RST.
    let read = tokio::time::timeout(Duration::from_secs(3), third.read_to_end(&mut response))
        .await
        .expect("the third connection was closed");
    assert!(response.is_empty(), "no payload, got {read:?}");
    assert_eq!(backend.connection_count(), 2, "the third never reached a backend");


    server.shutdown().await;
}

#[tokio::test]
async fn an_emptied_rate_bucket_produces_a_readable_kick() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let limits = "  max_connections_per_ip: 0\n  connection_rate:\n    burst: 2\n    per: 60s";
    let server = gateway(&config(&backend.address.to_string(), limits)).await;
    let address = server.address("public").unwrap();

    for name in ["Alice", "Bob"] {
        let _client = login(address, "survival.example.net", name).await;
    }
    backend.wait_for_connections(2).await;

    let mut refused = login(address, "survival.example.net", "Carol").await;
    assert_eq!(
        read_kick(&mut refused).await,
        "Slow down",
        "a rate-limited player should learn why, not just see a reset"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn a_backend_at_its_limit_reports_the_target_as_full() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
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
    max_connections: 1
messages:
  too_many_connections: "&cServer full"
timeouts:
  drain: 200ms
health:
  enabled: false
"#,
        backend.address
    );
    let server = gateway(&yaml).await;
    let address = server.address("public").unwrap();

    let _first = login(address, "survival.example.net", "Alice").await;
    backend.wait_for_connections(1).await;

    let mut second = login(address, "survival.example.net", "Bob").await;
    assert_eq!(read_kick(&mut second).await, "Server full");

    server.shutdown().await;
}

#[tokio::test]
async fn a_slot_freed_by_a_disconnect_is_reusable() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
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
    max_connections: 1
timeouts:
  drain: 200ms
health:
  enabled: false
"#,
        backend.address
    );
    let server = gateway(&yaml).await;
    let address = server.address("public").unwrap();
    let registry = server.app.runtime().registry.clone();

    let first = login(address, "survival.example.net", "Alice").await;
    backend.wait_for_connections(1).await;
    common::wait_until("the session to be counted", || {
        registry.backend("survival").unwrap().active_sessions() == 1
    })
    .await;

    drop(first);
    common::wait_until("the slot to be released", || {
        registry.backend("survival").unwrap().active_sessions() == 0
    })
    .await;

    let _second = login(address, "survival.example.net", "Bob").await;
    backend.wait_for_connections(2).await;

    server.shutdown().await;
}

#[tokio::test]
async fn an_oversized_handshake_is_cut_off() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let limits = "  max_handshake_bytes: 512";
    let server = gateway(&config(&backend.address.to_string(), limits)).await;
    let address = server.address("public").unwrap();

    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    // A length prefix that is legal on its own — below the packet cap — but
    // whose payload never arrives, so the handshake byte budget is what stops
    // it.
    let mut framed = Vec::new();
    mc_protocol::write_varint(&mut framed, 20_000);
    use tokio::io::AsyncWriteExt;
    stream.write_all(&framed).await.unwrap();

    let junk = vec![0x41u8; 256];
    let mut sent = 0usize;
    // Writing must start failing once the gateway gives up on us.
    while sent < 64 * 1024 {
        if stream.write_all(&junk).await.is_err() {
            break;
        }
        sent += junk.len();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response))
        .await
        .expect("the gateway hung up")
        .ok();
    assert_eq!(backend.connection_count(), 0);

    server.shutdown().await;
}

#[tokio::test]
async fn a_login_and_a_ping_both_reach_the_backend() {
    let backend = FakeBackend::start(Behaviour::Status(common::paper_status("hi", 0))).await;
    let server = gateway(&config(&backend.address.to_string(), "  max_connections_per_ip: 0")).await;
    let address = server.address("public").unwrap();

    let _client = login(address, "survival.example.net", "Alice").await;
    let _status = common::status_ping(address, "survival.example.net", 767).await;

    // Both the login and the status ping reach the backend: a ping is proxied
    // now, not answered locally.
    let seen = backend.wait_for_payload_connections(2).await;
    assert_eq!(seen.len(), 2);

    server.shutdown().await;
}
