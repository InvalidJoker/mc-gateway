//! The central MOTD: the gateway answers server list pings itself, which is
//! what keeps the public listing alive while backends restart.

mod common;

use common::{Behaviour, FakeBackend, gateway, login, status_ping, status_ping_with_pong};
use mc_protocol::legacy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(backend: &str) -> String {
    format!(
        r#"
listeners:
  - name: public
    bind: "127.0.0.1:0"
routing:
  default: survival
  rules:
    - host: "modded.example.net"
      target: survival
      motd:
        text: "&6Modded only"
        max_players: 50
servers:
  survival:
    address: "{backend}"
motd:
  enabled: true
  text: "&bMy Network"
  version_name: "MyNetwork"
  protocol: auto
  max_players: 1000
  players: sessions
  sample:
    - "&7welcome"
timeouts:
  drain: 200ms
health:
  enabled: false
"#
    )
}

#[tokio::test]
async fn the_gateway_answers_the_ping_itself() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string())).await;
    let address = server.address("public").unwrap();

    let status = status_ping(address, "survival.example.net", 767).await;

    assert_eq!(status["version"]["name"], "MyNetwork");
    assert_eq!(status["players"]["max"], 1000);
    assert_eq!(status["players"]["online"], 0);
    assert_eq!(status["description"]["text"], "\u{a7}bMy Network");
    assert_eq!(status["players"]["sample"][0]["name"], "\u{a7}7welcome");
    assert_eq!(backend.connection_count(), 0, "no backend was involved");

    server.shutdown().await;
}

#[tokio::test]
async fn the_reported_protocol_follows_the_client() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string())).await;
    let address = server.address("public").unwrap();

    // `protocol: auto` means a 1.8 client and a 1.21 client both see a version
    // they consider compatible, instead of the red cross in the server list.
    for protocol in [47, 340, 767] {
        let status = status_ping(address, "survival.example.net", protocol).await;
        assert_eq!(status["version"]["protocol"], protocol);
    }

    server.shutdown().await;
}

#[tokio::test]
async fn the_player_count_reflects_live_sessions() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string())).await;
    let address = server.address("public").unwrap();

    let _players: Vec<_> = futures_lite_join(vec![
        login(address, "survival.example.net", "Alice"),
        login(address, "survival.example.net", "Bob"),
    ])
    .await;
    backend.wait_for_connections(2).await;

    let status = status_ping(address, "survival.example.net", 767).await;
    assert_eq!(status["players"]["online"], 2);

    server.shutdown().await;
}

/// Small helper so the test does not pull in a futures crate.
async fn futures_lite_join<F: std::future::Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut out = Vec::new();
    for future in futures {
        out.push(future.await);
    }
    out
}

#[tokio::test]
async fn a_route_can_override_the_motd() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string())).await;
    let address = server.address("public").unwrap();

    let status = status_ping(address, "modded.example.net", 767).await;
    assert_eq!(status["description"]["text"], "\u{a7}6Modded only");
    assert_eq!(status["players"]["max"], 50);
    // Unset fields still come from the global MOTD.
    assert_eq!(status["version"]["name"], "MyNetwork");

    server.shutdown().await;
}

#[tokio::test]
async fn the_ping_is_answered_with_the_clients_own_payload() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string())).await;
    let address = server.address("public").unwrap();

    let (_, pong) = status_ping_with_pong(address, "survival.example.net").await;
    assert_eq!(pong, 0x1234_5678, "the client measures latency from this value");

    server.shutdown().await;
}

#[tokio::test]
async fn a_down_target_shows_the_offline_motd() {
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
motd:
  text: "&bMy Network"
  offline:
    text: "&cMaintenance"
    mark_incompatible: true
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

    common::wait_until("the backend to go down", || {
        !registry.backend("dead").unwrap().is_healthy()
    })
    .await;

    let status = status_ping(address, "anything.example.net", 767).await;
    assert_eq!(status["description"]["text"], "\u{a7}cMaintenance");
    assert_eq!(status["players"]["online"], 0);
    assert_eq!(status["version"]["protocol"], -1, "renders as incompatible");

    server.shutdown().await;
}

#[tokio::test]
async fn an_unroutable_host_still_gets_an_answer() {
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
motd:
  offline:
    text: "&cNo such server"
timeouts:
  drain: 200ms
health:
  enabled: false
"#,
        backend.address
    );
    let server = gateway(&yaml).await;
    let address = server.address("public").unwrap();

    // A typo'd hostname is answered honestly rather than dropped.
    let status = status_ping(address, "typo.example.net", 767).await;
    assert_eq!(status["description"]["text"], "\u{a7}cNo such server");

    server.shutdown().await;
}

#[tokio::test]
async fn disabling_the_motd_proxies_the_ping_to_the_backend() {
    let json = r#"{"version":{"name":"Paper 1.21.1","protocol":767},
                   "players":{"max":20,"online":3},"description":{"text":"backend motd"}}"#;
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

    let status = status_ping(address, "survival.example.net", 767).await;
    assert_eq!(status["description"]["text"], "backend motd");
    assert_eq!(status["players"]["online"], 3);
    assert_eq!(backend.connection_count(), 1, "the ping really went upstream");

    server.shutdown().await;
}

#[tokio::test]
async fn pre_1_7_clients_get_a_legacy_reply() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string())).await;
    let address = server.address("public").unwrap();

    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    // A 1.6 client opens with 0xFE 0x01 and no VarInt framing at all.
    stream.write_all(&[legacy::LEGACY_PING_BYTE, 0x01]).await.unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();

    assert_eq!(response[0], legacy::LEGACY_KICK_ID);
    let units: Vec<u16> =
        response[3..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    let text = String::from_utf16(&units).unwrap();
    let fields: Vec<&str> = text.split('\u{0}').collect();

    assert_eq!(fields[0], "\u{a7}1");
    assert_eq!(fields[2], "MyNetwork");
    assert_eq!(fields[3], "My Network", "formatting codes are stripped for old clients");
    assert_eq!(fields[5], "1000");

    server.shutdown().await;
}
