//! Status pings.
//!
//! The gateway forwards the ping to the backend and returns the backend's own
//! answer. The only thing it may change is the MOTD text, and only the lines
//! the config names — everything else about the response has to arrive intact.

mod common;

use common::{Behaviour, FakeBackend, gateway, paper_status, status_ping, status_ping_with_pong};
use mc_protocol::legacy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn config(backend: &str, motd: &str) -> String {
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
        line2: "&6Modpack v4 required"
servers:
  survival:
    address: "{backend}"
{motd}
timeouts:
  drain: 200ms
health:
  enabled: false
"#
    )
}

const REWRITE_SECOND: &str = "motd:\n  line2: \"&7survival &8• &7creative\"";
const PASS_THROUGH: &str = "";

#[tokio::test]
async fn without_configured_lines_the_response_is_untouched() {
    let json = paper_status("A Minecraft Server", 7);
    let backend = FakeBackend::start(Behaviour::Status(json)).await;
    let server = gateway(&config(&backend.address.to_string(), PASS_THROUGH)).await;
    let address = server.address("public").unwrap();

    let status = status_ping(address, "survival.example.net", 767).await;

    assert_eq!(status["version"]["name"], "Paper 1.21.1");
    assert_eq!(status["version"]["protocol"], 767);
    assert_eq!(status["players"]["online"], 7);
    assert_eq!(status["description"]["text"], "A Minecraft Server");
    assert_eq!(status["favicon"], "data:image/png;base64,AAAA");
    assert_eq!(backend.connection_count(), 1, "the ping really went upstream");

    server.shutdown().await;
}

#[tokio::test]
async fn only_the_second_line_changes() {
    let json = paper_status("A Minecraft Server\\nsecond line from the backend", 7);
    let backend = FakeBackend::start(Behaviour::Status(json)).await;
    let server = gateway(&config(&backend.address.to_string(), REWRITE_SECOND)).await;
    let address = server.address("public").unwrap();

    let status = status_ping(address, "survival.example.net", 767).await;
    let text = status["description"]["text"].as_str().unwrap();
    let lines: Vec<&str> = text.split('\n').collect();

    assert_eq!(lines[0], "A Minecraft Server", "the backend keeps its first line");
    assert_eq!(lines[1], "\u{a7}7survival \u{a7}8• \u{a7}7creative");

    // Everything that is not the MOTD still comes from the backend.
    assert_eq!(status["version"]["name"], "Paper 1.21.1");
    assert_eq!(status["players"]["online"], 7);
    assert_eq!(status["players"]["max"], 100);
    assert_eq!(status["players"]["sample"][0]["name"], "Notch");
    assert_eq!(status["favicon"], "data:image/png;base64,AAAA");
    assert_eq!(status["enforcesSecureChat"], false);

    server.shutdown().await;
}

#[tokio::test]
async fn player_counts_come_from_the_backend_not_the_gateway() {
    let backend = FakeBackend::start(Behaviour::Status(paper_status("hi", 42))).await;
    let server = gateway(&config(&backend.address.to_string(), REWRITE_SECOND)).await;
    let address = server.address("public").unwrap();

    let status = status_ping(address, "survival.example.net", 767).await;
    assert_eq!(status["players"]["online"], 42, "no session of ours, but 42 real players");

    server.shutdown().await;
}

#[tokio::test]
async fn a_backend_with_one_line_gains_the_configured_second() {
    let backend = FakeBackend::start(Behaviour::Status(paper_status("only one line", 0))).await;
    let server = gateway(&config(&backend.address.to_string(), REWRITE_SECOND)).await;
    let address = server.address("public").unwrap();

    let status = status_ping(address, "survival.example.net", 767).await;
    let text = status["description"]["text"].as_str().unwrap();
    assert_eq!(text, "only one line\n\u{a7}7survival \u{a7}8• \u{a7}7creative");

    server.shutdown().await;
}

#[tokio::test]
async fn a_route_can_set_its_own_line() {
    let backend = FakeBackend::start(Behaviour::Status(paper_status("Backend MOTD", 0))).await;
    let server = gateway(&config(&backend.address.to_string(), REWRITE_SECOND)).await;
    let address = server.address("public").unwrap();

    let status = status_ping(address, "modded.example.net", 767).await;
    let text = status["description"]["text"].as_str().unwrap();
    assert_eq!(text, "Backend MOTD\n\u{a7}6Modpack v4 required");

    server.shutdown().await;
}

#[tokio::test]
async fn the_latency_ping_survives_the_rewrite() {
    let backend = FakeBackend::start(Behaviour::Status(paper_status("hi", 0))).await;
    let server = gateway(&config(&backend.address.to_string(), REWRITE_SECOND)).await;
    let address = server.address("public").unwrap();

    // The gateway intercepts the status response and then goes back to being a
    // pipe, so the ping that follows still reaches the backend.
    let (_, pong) = status_ping_with_pong(address, "survival.example.net").await;
    assert_eq!(pong, 0x1234_5678);

    server.shutdown().await;
}

#[tokio::test]
async fn a_down_target_falls_back_to_the_offline_motd() {
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
  line2: "&7ignored while offline"
  offline:
    text: "&cMaintenance"
    version_name: "maintenance"
    max_players: 0
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

    // There is nothing to pass through, so this is the one response the gateway
    // makes up on its own.
    let status = status_ping(address, "anything.example.net", 767).await;
    assert_eq!(status["description"]["text"], "\u{a7}cMaintenance");
    assert_eq!(status["version"]["name"], "maintenance");
    assert_eq!(status["version"]["protocol"], -1, "renders as incompatible");
    assert_eq!(status["players"]["online"], 0);

    server.shutdown().await;
}

#[tokio::test]
async fn an_unroutable_host_gets_the_offline_answer() {
    let backend = FakeBackend::start(Behaviour::Status(paper_status("hi", 0))).await;
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

    let status = status_ping(address, "typo.example.net", 767).await;
    assert_eq!(status["description"]["text"], "\u{a7}cNo such server");
    assert_eq!(backend.connection_count(), 0, "an unknown host reaches no backend");

    server.shutdown().await;
}

#[tokio::test]
async fn a_status_response_the_gateway_cannot_parse_is_forwarded_anyway() {
    // A backend answering with something that is not a status document at all.
    let backend = FakeBackend::start(Behaviour::Status("not json".to_owned())).await;
    let server = gateway(&config(&backend.address.to_string(), REWRITE_SECOND)).await;
    let address = server.address("public").unwrap();

    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let mut request =
        common::handshake(767, "survival.example.net", 25565, mc_protocol::NextState::Status);
    request.extend_from_slice(&mc_protocol::encode_packet(0x00, &[]));
    stream.write_all(&request).await.unwrap();

    let (id, body) = common::read_packet(&mut stream).await.expect("a response");
    let payload = mc_protocol::Reader::new(&body).string(262_144).unwrap();
    assert_eq!(id, 0x00);
    assert_eq!(payload, "not json", "passed through rather than dropped");

    server.shutdown().await;
}

#[tokio::test]
async fn pre_1_7_pings_are_forwarded_to_the_default_target() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(&backend.address.to_string(), REWRITE_SECOND)).await;
    let address = server.address("public").unwrap();

    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    // A 1.6 client opens with 0xFE 0x01 and no VarInt framing at all.
    stream.write_all(&[legacy::LEGACY_PING_BYTE, 0x01]).await.unwrap();

    // The backend receives the legacy ping verbatim: the gateway does not
    // answer these itself, it hands them to a server that still can.
    let seen = backend.wait_for_payload_connections(1).await;
    assert_eq!(seen[0].head, vec![legacy::LEGACY_PING_BYTE, 0x01]);

    // And the pipe is live, so the backend's reply would reach the client.
    stream.write_all(b"more").await.unwrap();
    let mut echoed = [0u8; 4];
    tokio::time::timeout(std::time::Duration::from_secs(5), stream.read_exact(&mut echoed))
        .await
        .expect("the session is piping")
        .unwrap();
    assert_eq!(&echoed, b"more");

    server.shutdown().await;
}

#[tokio::test]
async fn a_client_that_sends_the_request_separately_still_gets_the_motd() {
    // The real Minecraft client flushes the handshake and the status request
    // as two separate writes. The gateway must not start waiting for the
    // backend's answer before the backend has even been asked.
    let backend = FakeBackend::start(Behaviour::Status(paper_status("Backend MOTD", 3))).await;
    let server = gateway(&config(&backend.address.to_string(), REWRITE_SECOND)).await;
    let address = server.address("public").unwrap();

    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    stream
        .write_all(&common::handshake(767, "survival.example.net", 25565, mc_protocol::NextState::Status))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    stream.write_all(&mc_protocol::encode_packet(0x00, &[])).await.unwrap();

    let (id, body) = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        common::read_packet(&mut stream),
    )
    .await
    .expect("a status response within 3s, not a stall until the status timeout")
    .expect("a status response");
    assert_eq!(id, 0x00);

    let json: serde_json::Value =
        serde_json::from_str(&mc_protocol::Reader::new(&body).string(262_144).unwrap()).unwrap();
    assert_eq!(json["description"]["text"], "Backend MOTD\n\u{a7}7survival \u{a7}8• \u{a7}7creative");
    assert_eq!(json["players"]["online"], 3);

    server.shutdown().await;
}

#[tokio::test]
async fn an_unreachable_backend_still_answers_the_server_list() {
    // Health checks disabled, so the backend counts as up — but nothing
    // listens there. The ping must get the offline MOTD, not a login
    // disconnect that the server list cannot parse.
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
  line2: "&7via the gateway"
  offline:
    text: "&cbackend offline"
timeouts:
  connect: 500ms
  drain: 200ms
health:
  enabled: false
"#
    );
    let server = gateway(&yaml).await;
    let address = server.address("public").unwrap();

    let status = status_ping(address, "anything.example.net", 767).await;
    assert_eq!(status["description"]["text"], "\u{a7}cbackend offline");
    assert!(status.get("version").is_some(), "a complete status document");

    server.shutdown().await;
}
