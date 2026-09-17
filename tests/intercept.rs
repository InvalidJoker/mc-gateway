//! Intercepted connections, driven without TPROXY: connections are accepted on
//! a plain listener and handed to the intercept handler with the fake server's
//! address as the "original destination" — exactly what TPROXY reports.
//!
//! The kernel side (rules, Docker's DNAT, IPv6, fail-open) is covered by
//! dev/check.sh.

mod common;

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use common::{Behaviour, FakeServer, handshake, login_start, paper_status, read_packet};
use mc_gateway::{
    config::Config,
    intercept,
    protocol::{NextState, PING_ID, Reader, STATUS_REQUEST_ID, Writer, encode_packet},
    server::Gateway,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};

const WITH_AD: &str = r#"
ports: ["30000-40000"]
motd:
  line2: "&7Hosted by &bexample.net"
timeouts:
  handshake: 2s
  connect: 500ms
"#;

const WITHOUT_AD: &str = r#"
ports: ["30000-40000"]
timeouts:
  handshake: 2s
"#;

/// A listener standing in for the TPROXY one, pointed at `server`.
async fn node(gateway: Arc<Gateway>, server: SocketAddr) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, peer) = listener.accept().await.unwrap();
            tokio::spawn(intercept::handle(stream, peer, server, Arc::clone(&gateway)));
        }
    });
    address
}

fn gateway(config: &str) -> Arc<Gateway> {
    Gateway::new(Config::parse(config, "test").unwrap().config, PathBuf::from("test.yaml"))
}

/// A status ping the way the real client sends it: handshake, pause, request.
async fn status_ping(address: SocketAddr) -> serde_json::Value {
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&handshake(NextState::Status)).await.unwrap();
    time::sleep(Duration::from_millis(50)).await;
    stream.write_all(&encode_packet(STATUS_REQUEST_ID, &[])).await.unwrap();

    let (_, body) = time::timeout(Duration::from_secs(5), read_packet(&mut stream))
        .await
        .expect("a status response, promptly")
        .expect("a status response");
    serde_json::from_str(&Reader::new(&body).string(4 * 1024 * 1024).unwrap()).unwrap()
}

#[tokio::test]
async fn the_ad_replaces_line_two_and_nothing_else() {
    let customer =
        FakeServer::start(Behaviour::Status(paper_status("Steve's SMP\\nwhitelist on", 4))).await;
    let address = node(gateway(WITH_AD), customer.address).await;

    let status = status_ping(address).await;
    let text = status["description"]["text"].as_str().unwrap();

    assert_eq!(text, "Steve's SMP\n\u{a7}7Hosted by \u{a7}bexample.net");
    assert_eq!(status["players"]["online"], 4);
    assert_eq!(status["version"]["name"], "Paper 1.21.1");
    assert_eq!(status["favicon"], "data:image/png;base64,AAAA");
}

#[tokio::test]
async fn without_an_ad_the_response_is_byte_identical() {
    let json = paper_status("Steve's SMP", 4);
    let customer = FakeServer::start(Behaviour::Status(json.clone())).await;
    let address = node(gateway(WITHOUT_AD), customer.address).await;

    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&handshake(NextState::Status)).await.unwrap();
    stream.write_all(&encode_packet(STATUS_REQUEST_ID, &[])).await.unwrap();
    let (_, body) = read_packet(&mut stream).await.unwrap();

    assert_eq!(Reader::new(&body).string(1 << 20).unwrap(), json);
}

#[tokio::test]
async fn a_login_reaches_the_server_verbatim() {
    let customer = FakeServer::start(Behaviour::Echo).await;
    let address = node(gateway(WITH_AD), customer.address).await;

    let mut sent = handshake(NextState::Login);
    sent.extend_from_slice(&login_start("Steve"));
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&sent).await.unwrap();

    let seen = customer.wait_for(1).await;
    assert_eq!(seen[0], sent, "not a byte added, removed or changed");

    stream.write_all(b"play").await.unwrap();
    let mut echoed = [0u8; 4];
    time::timeout(Duration::from_secs(5), stream.read_exact(&mut echoed)).await.unwrap().unwrap();
    assert_eq!(&echoed, b"play");
}

#[tokio::test]
async fn a_server_that_speaks_first_is_not_kept_waiting() {
    // SSH, FTP and friends: the client says nothing until the server has.
    let customer = FakeServer::start(Behaviour::Banner(b"SSH-2.0-OpenSSH_9.9\r\n".to_vec())).await;
    let address = node(gateway(WITH_AD), customer.address).await;

    let mut stream = TcpStream::connect(address).await.unwrap();
    let mut banner = vec![0u8; 21];
    // Well under the 2s handshake timeout: the gateway must notice the server
    // talking rather than wait the client out.
    time::timeout(Duration::from_millis(1000), stream.read_exact(&mut banner))
        .await
        .expect("banner arrives without waiting for a timeout")
        .unwrap();
    assert_eq!(banner, b"SSH-2.0-OpenSSH_9.9\r\n");
}

#[tokio::test]
async fn a_protocol_that_is_not_minecraft_passes_through() {
    let customer = FakeServer::start(Behaviour::Echo).await;
    let address = node(gateway(WITH_AD), customer.address).await;

    let request = b"GET /status HTTP/1.1\r\nHost: node.example.net\r\n\r\n".to_vec();
    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&request).await.unwrap();

    let started = time::Instant::now();
    let seen = customer.wait_for(1).await;
    assert_eq!(seen[0], request);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "forwarded at once, not after the handshake timeout ({:?})",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_pre_1_7_ping_passes_through() {
    let customer = FakeServer::start(Behaviour::Echo).await;
    let address = node(gateway(WITH_AD), customer.address).await;

    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&[0xFE, 0x01]).await.unwrap();
    let seen = customer.wait_for(1).await;
    assert_eq!(seen[0], vec![0xFE, 0x01]);
}

#[tokio::test]
async fn a_stopped_server_looks_like_a_closed_port() {
    let stopped = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
    let address = node(gateway(WITH_AD), stopped).await;

    let mut stream = TcpStream::connect(address).await.unwrap();
    let mut buf = Vec::new();
    // No MOTD invented, no kick message: the connection simply ends.
    let read = time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf))
        .await
        .expect("closed promptly");
    assert!(buf.is_empty(), "nothing was sent, got {read:?}");
}

#[tokio::test]
async fn a_modpack_sized_status_response_still_gets_the_ad() {
    // Forge lists every mod in its status response; hundreds of them add up.
    let mods: Vec<String> = (0..6000)
        .map(|i| format!(r#"{{"modId":"examplemod{i}","modmarker":"1.0.0-release"}}"#))
        .collect();
    let json = format!(
        r#"{{"version":{{"name":"1.12.2","protocol":340}},"players":{{"max":20,"online":0}},
            "description":{{"text":"Modpack"}},"modinfo":{{"type":"FML","modList":[{}]}}}}"#,
        mods.join(",")
    );
    assert!(json.len() > 256 * 1024, "the test needs a response over the old limit");
    let customer = FakeServer::start(Behaviour::Status(json)).await;
    let address = node(gateway(WITH_AD), customer.address).await;

    let status = status_ping(address).await;
    assert_eq!(status["description"]["text"], "Modpack\n\u{a7}7Hosted by \u{a7}bexample.net");
    assert_eq!(status["modinfo"]["modList"].as_array().unwrap().len(), 6000);
}

#[tokio::test]
async fn an_unparseable_status_response_is_forwarded_as_is() {
    let customer = FakeServer::start(Behaviour::Status("definitely not json".into())).await;
    let address = node(gateway(WITH_AD), customer.address).await;

    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&handshake(NextState::Status)).await.unwrap();
    stream.write_all(&encode_packet(STATUS_REQUEST_ID, &[])).await.unwrap();
    let (_, body) = read_packet(&mut stream).await.unwrap();
    assert_eq!(Reader::new(&body).string(1 << 20).unwrap(), "definitely not json");
}

#[tokio::test]
async fn the_latency_ping_after_the_ad_still_works() {
    let customer = FakeServer::start(Behaviour::Status(paper_status("hi", 0))).await;
    let address = node(gateway(WITH_AD), customer.address).await;

    let mut stream = TcpStream::connect(address).await.unwrap();
    stream.write_all(&handshake(NextState::Status)).await.unwrap();
    stream.write_all(&encode_packet(STATUS_REQUEST_ID, &[])).await.unwrap();
    read_packet(&mut stream).await.expect("status response");

    let mut ping = Writer::new();
    ping.i64(987_654_321);
    stream.write_all(&encode_packet(PING_ID, ping.as_slice())).await.unwrap();
    let (id, body) = read_packet(&mut stream).await.expect("pong");
    assert_eq!(id, PING_ID);
    assert_eq!(Reader::new(&body).i64().unwrap(), 987_654_321);
}

#[tokio::test]
async fn a_reload_changes_the_line_for_new_pings() {
    let dir = std::env::temp_dir().join(format!("mc-gateway-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(&path, WITH_AD).unwrap();

    let gateway = Gateway::new(Config::load(&path).unwrap().config, path.clone());
    let customer = FakeServer::start(Behaviour::Status(paper_status("Steve's SMP", 1))).await;
    let address = node(Arc::clone(&gateway), customer.address).await;
    assert!(status_ping(address).await["description"]["text"].as_str().unwrap().contains("example.net"));

    std::fs::write(&path, WITH_AD.replace("example.net", "other.example")).unwrap();
    let warnings = gateway.reload().unwrap();
    assert!(!warnings.iter().any(|w| w.contains("restart")), "{warnings:?}");
    assert!(status_ping(address).await["description"]["text"].as_str().unwrap().contains("other.example"));

    std::fs::write(&path, WITH_AD.replace("30000-40000", "30000-30500")).unwrap();
    let warnings = gateway.reload().unwrap();
    assert!(warnings.iter().any(|w| w.contains("restart")), "a port change needs a restart: {warnings:?}");

    std::fs::write(&path, "ports: [").unwrap();
    assert!(gateway.reload().is_err(), "a broken file is rejected");
    assert!(status_ping(address).await["description"]["text"].as_str().is_some(), "and the gateway keeps working");
    std::fs::remove_dir_all(&dir).unwrap();
}
