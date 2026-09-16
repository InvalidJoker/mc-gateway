//! Config reload and graceful shutdown.

mod common;

use std::{path::PathBuf, time::Duration};

use common::{Behaviour, FakeBackend, gateway_from_file, login, read_some};
use tokio::io::AsyncWriteExt;

fn config_file(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("mc-gateway-reload-tests");
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(format!("{name}.yaml"));
    std::fs::write(&path, body).expect("write config");
    path
}

fn config(backend: &str) -> String {
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
timeouts:
  drain: 200ms
health:
  enabled: false
"#
    )
}

#[tokio::test]
async fn a_reload_reroutes_new_sessions() {
    let first = FakeBackend::start(Behaviour::Echo).await;
    let second = FakeBackend::start(Behaviour::Echo).await;

    let path = config_file("reroute", &config(&first.address.to_string()));
    let server = gateway_from_file(&path).await;
    let address = server.address("public").unwrap();

    let _before = login(address, "survival.example.net", "Alice").await;
    first.wait_for_connections(1).await;

    std::fs::write(&path, config(&second.address.to_string())).expect("rewrite config");
    server.app.reload().await.expect("reload succeeds");

    let _after = login(address, "survival.example.net", "Bob").await;
    second.wait_for_connections(1).await;
    assert_eq!(first.connection_count(), 1, "the old backend got no new sessions");

    server.shutdown().await;
}

#[tokio::test]
async fn an_established_session_survives_a_reload() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let other = FakeBackend::start(Behaviour::Echo).await;

    let path = config_file("survive", &config(&backend.address.to_string()));
    let server = gateway_from_file(&path).await;
    let address = server.address("public").unwrap();

    let mut client = login(address, "survival.example.net", "Alice").await;
    backend.wait_for_connections(1).await;

    std::fs::write(&path, config(&other.address.to_string())).expect("rewrite config");
    server.app.reload().await.expect("reload succeeds");

    // The pipe belongs to the old routing snapshot and must keep working.
    client.write_all(b"still here").await.unwrap();
    assert_eq!(read_some(&mut client).await, b"still here");

    server.shutdown().await;
}

#[tokio::test]
async fn a_broken_config_leaves_the_running_one_in_place() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let path = config_file("broken", &config(&backend.address.to_string()));
    let server = gateway_from_file(&path).await;
    let address = server.address("public").unwrap();

    std::fs::write(&path, "listeners:\n  - name: public\n    bind: \"not-an-address\"\n")
        .expect("rewrite config");
    let err = server.app.reload().await.expect_err("the reload must fail");
    assert!(err.contains("parse") || err.contains("invalid"), "{err}");

    // Routing still works, because nothing was swapped in.
    let _client = login(address, "survival.example.net", "Alice").await;
    backend.wait_for_connections(1).await;

    server.shutdown().await;
}

#[tokio::test]
async fn health_state_is_carried_across_a_reload() {
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
    let path = config_file("health", &yaml);
    let server = gateway_from_file(&path).await;

    let registry = server.app.runtime().registry.clone();
    common::wait_until("the backend to go down", || {
        !registry.backend("dead").unwrap().is_healthy()
    })
    .await;

    server.app.reload().await.expect("reload succeeds");

    // A reload must not hand every backend a clean bill of health.
    let reloaded = server.app.runtime().registry.clone();
    assert!(!reloaded.backend("dead").unwrap().is_healthy());

    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_stops_accepting_but_lets_sessions_finish() {
    let backend = FakeBackend::start(Behaviour::Echo).await;
    let path = config_file("drain", &config(&backend.address.to_string()));
    let server = gateway_from_file(&path).await;
    let address = server.address("public").unwrap();

    let mut client = login(address, "survival.example.net", "Alice").await;
    backend.wait_for_connections(1).await;

    server.app.begin_shutdown();

    // New connections are no longer served: the listening socket is gone, so
    // the kernel refuses them outright.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let refused = tokio::net::TcpStream::connect(address).await;
    assert!(refused.is_err(), "the listener should be closed, got {refused:?}");

    // ...while the established one still carries traffic.
    client.write_all(b"in flight").await.unwrap();
    assert_eq!(read_some(&mut client).await, b"in flight");

    server.shutdown().await;
}
