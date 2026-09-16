//! Group selection policies, exercised through real connections.

mod common;

use common::{Behaviour, FakeBackend, gateway, login};

fn config(policy: &str, first: &str, second: &str, health: &str) -> String {
    format!(
        r#"
listeners:
  - name: public
    bind: "127.0.0.1:0"
routing:
  default: survival
servers:
  survival-01:
    address: "{first}"
    group: survival
  survival-02:
    address: "{second}"
    group: survival
groups:
  survival:
    policy: {policy}
timeouts:
  drain: 200ms
health:
{health}
"#
    )
}

const NO_HEALTH: &str = "  enabled: false";
const FAST_HEALTH: &str = "  method: tcp\n  interval: 30ms\n  timeout: 20ms\n  fall: 1\n  rise: 1";

#[tokio::test]
async fn round_robin_spreads_sessions_across_the_group() {
    let first = FakeBackend::start(Behaviour::Echo).await;
    let second = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(
        "round_robin",
        &first.address.to_string(),
        &second.address.to_string(),
        NO_HEALTH,
    ))
    .await;
    let address = server.address("public").unwrap();

    let mut clients = Vec::new();
    for i in 0..6 {
        clients.push(login(address, "survival.example.net", &format!("P{i}")).await);
    }
    first.wait_for_connections(3).await;
    second.wait_for_connections(3).await;

    assert_eq!(first.connection_count(), 3);
    assert_eq!(second.connection_count(), 3);

    server.shutdown().await;
}

#[tokio::test]
async fn least_connections_fills_the_quieter_backend() {
    let first = FakeBackend::start(Behaviour::Echo).await;
    let second = FakeBackend::start(Behaviour::Echo).await;
    let server = gateway(&config(
        "least_connections",
        &first.address.to_string(),
        &second.address.to_string(),
        NO_HEALTH,
    ))
    .await;
    let address = server.address("public").unwrap();
    let registry = server.app.runtime().registry.clone();

    let mut clients = Vec::new();
    for i in 0..4 {
        clients.push(login(address, "survival.example.net", &format!("P{i}")).await);
        // Sessions must be counted before the next pick, or every choice would
        // be made against the same zeroed state.
        common::wait_until("the session to register", || {
            registry.target("survival").unwrap().active_sessions() == i + 1
        })
        .await;
    }

    assert_eq!(first.connection_count(), 2);
    assert_eq!(second.connection_count(), 2);

    server.shutdown().await;
}

#[tokio::test]
async fn a_dead_member_is_skipped_once_health_checks_notice() {
    let alive = FakeBackend::start(Behaviour::Echo).await;
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();

    let server = gateway(&config(
        "round_robin",
        &dead.to_string(),
        &alive.address.to_string(),
        FAST_HEALTH,
    ))
    .await;
    let address = server.address("public").unwrap();
    let registry = server.app.runtime().registry.clone();

    common::wait_until("the dead member to be marked down", || {
        !registry.backend("survival-01").unwrap().is_healthy()
    })
    .await;

    let mut clients = Vec::new();
    for i in 0..4 {
        clients.push(login(address, "survival.example.net", &format!("P{i}")).await);
    }
    // Health checks also connect, so only sessions that sent a handshake count.
    alive.wait_for_payload_connections(4).await;
    assert_eq!(
        alive.payload_connections().len(),
        4,
        "every session went to the healthy member"
    );

    server.shutdown().await;
}
