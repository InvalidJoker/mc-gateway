//! The Prometheus endpoint.
//!
//! One test in its own binary: installing the exporter sets a process-wide
//! recorder, so it can only happen once.

mod common;

use std::time::Duration;

use common::{Behaviour, FakeBackend, gateway, login, paper_status};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn scrape(address: std::net::SocketAddr) -> String {
    let mut stream = TcpStream::connect(address).await.expect("connect to /metrics");
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut body = String::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut body))
        .await
        .expect("scrape completed")
        .unwrap();
    body
}

#[tokio::test]
async fn traffic_shows_up_on_the_scrape_endpoint() {
    let backend = FakeBackend::start(Behaviour::Status(paper_status("hi", 3))).await;

    // Reserve a port the same way the listeners do.
    let metrics_port = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();

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
motd:
  line2: "&7via the gateway"
metrics:
  enabled: true
  bind: "127.0.0.1:{metrics_port}"
timeouts:
  drain: 200ms
health:
  method: tcp
  interval: 50ms
  timeout: 20ms
  rise: 1
  fall: 1
"#,
        backend.address
    );

    let server = gateway(&yaml).await;
    let address = server.address("public").unwrap();

    let _player = login(address, "survival.example.net", "Alice").await;
    let _status = common::status_ping(address, "survival.example.net", 767).await;
    backend.wait_for_payload_connections(2).await;

    // Registry gauges are sampled on a timer, so give it one tick.
    tokio::time::sleep(Duration::from_secs(6)).await;
    let body = scrape(format!("127.0.0.1:{metrics_port}").parse().unwrap()).await;

    assert!(body.starts_with("HTTP/1.1 200"), "{body}");
    for expected in [
        "mc_gateway_connections_total",
        "mc_gateway_login_attempts_total",
        "mc_gateway_backend_connections_total",
        "mc_gateway_motd_rewrites_total",
        "mc_gateway_backend_up",
        "mc_gateway_backends_healthy",
    ] {
        assert!(body.contains(expected), "missing {expected} in:\n{body}");
    }

    // Labels survive the trip.
    assert!(body.contains(r#"backend="survival""#), "{body}");
    assert!(body.contains(r#"kind="status""#), "{body}");

    server.shutdown().await;
}
