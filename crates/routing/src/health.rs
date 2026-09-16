//! Background health checks.
//!
//! The point is not to know precisely when a server died, it is to keep dead
//! servers out of the selection pool and to keep the public MOTD answering even
//! when every backend is gone.

use std::sync::Arc;

use mc_config::HealthMethod;
use mc_forwarding::Origin;
use tokio::{sync::watch, task::JoinSet, time};
use tracing::{debug, info, warn};

use crate::registry::{Backend, Registry, StatusSnapshot};

/// Spawns one checker per backend and returns when `shutdown` flips or all
/// checkers stop.
///
/// Checks are spread across the interval rather than fired together, so 300
/// backends do not all get probed in the same millisecond.
pub async fn run(registry: Arc<Registry>, mut shutdown: watch::Receiver<bool>) {
    let mut tasks = JoinSet::new();
    let total = registry.backends().len();

    for (index, backend) in registry.backends().enumerate() {
        if !backend.health_config.enabled {
            debug!(backend = %backend.name, "health checks disabled");
            continue;
        }
        let backend = Arc::clone(backend);
        let shutdown = shutdown.clone();
        let offset = if total > 0 {
            backend.health_config.interval.get().mul_f64(index as f64 / total as f64)
        } else {
            std::time::Duration::ZERO
        };
        tasks.spawn(check_loop(backend, offset, shutdown));
    }

    if tasks.is_empty() {
        return;
    }

    tokio::select! {
        _ = async { while tasks.join_next().await.is_some() {} } => {}
        _ = wait_for_shutdown(&mut shutdown) => {
            tasks.abort_all();
        }
    }
}

/// Waits until the shutdown flag is set.
///
/// `watch::Receiver::wait_for` hands back a borrow guard that is not `Send`,
/// which would make every task holding it unspawnable; this drops the guard
/// before awaiting.
async fn wait_for_shutdown(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn check_loop(
    backend: Arc<Backend>,
    offset: std::time::Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    if !offset.is_zero() {
        tokio::select! {
            _ = time::sleep(offset) => {}
            _ = wait_for_shutdown(&mut shutdown) => return,
        }
    }

    let mut ticker = time::interval(backend.health_config.interval.get());
    // A slow backend must not cause a burst of catch-up probes.
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let outcome = check_once(&backend).await;
                let ok = outcome.is_ok();
                let snapshot = outcome.as_ref().ok().and_then(Clone::clone);

                if let Some(now_healthy) = backend.record_health(ok, snapshot) {
                    if now_healthy {
                        info!(
                            backend = %backend.name,
                            address = %backend.address,
                            "backend is healthy again"
                        );
                    } else {
                        warn!(
                            backend = %backend.name,
                            address = %backend.address,
                            reason = outcome.as_ref().err().map(String::as_str).unwrap_or("unknown"),
                            "backend marked down"
                        );
                    }
                } else if let Err(reason) = &outcome {
                    debug!(backend = %backend.name, %reason, "health check failed");
                }
            }
            _ = wait_for_shutdown(&mut shutdown) => return,
        }
    }
}

/// One probe. `Ok(None)` means a TCP-only check succeeded.
async fn check_once(backend: &Backend) -> Result<Option<StatusSnapshot>, String> {
    let timeout = backend.health_config.timeout.get();

    let address = mc_forwarding::resolve(&backend.address).await.map_err(|e| e.to_string())?;
    // The probe uses the same forwarding mode as real sessions, so a backend
    // that only accepts PROXY-protocol connections is checked the way it is
    // actually used. `Origin::Gateway` makes that a LOCAL header, never a
    // fabricated client address.
    let mut stream =
        mc_forwarding::connect(address, backend.forwarding, Origin::Gateway, timeout)
            .await
            .map_err(|e| e.to_string())?;

    match backend.health_config.method {
        HealthMethod::Tcp => Ok(None),
        HealthMethod::Status => {
            let (host, port) = mc_config::split_host_port(&backend.address)?;
            crate::ping::status_ping(&mut stream, host, port, timeout)
                .await
                .map(Some)
                .map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_config::Config;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn a_dead_backend_is_taken_out_of_rotation() {
        // Bind then drop, so the port is closed but syntactically valid.
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();

        let yaml = format!(
            r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  default: one
servers:
  one:
    address: "{dead}"
health:
  method: tcp
  interval: 20ms
  timeout: 10ms
  fall: 1
  rise: 1
"#
        );
        let config = Config::parse(&yaml, "test").unwrap().config;
        let registry = Registry::build(Arc::new(config));
        let backend = Arc::clone(registry.backend("one").unwrap());
        assert!(backend.is_healthy(), "starts healthy");

        let (tx, rx) = watch::channel(false);
        let checker = tokio::spawn(run(Arc::clone(&registry), rx));

        for _ in 0..100 {
            if !backend.is_healthy() {
                break;
            }
            time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!backend.is_healthy(), "an unreachable backend must be marked down");

        tx.send(true).unwrap();
        checker.await.unwrap();
    }

    #[tokio::test]
    async fn a_live_backend_stays_healthy_and_reports_players() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 512];
                    let _ = stream.read(&mut buf).await;
                    let json = r#"{"version":{"name":"Paper","protocol":767},
                                   "players":{"max":50,"online":12}}"#;
                    let _ = stream
                        .write_all(&mc_protocol::status::encode_status_response(json))
                        .await;
                });
            }
        });

        let yaml = format!(
            r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  default: one
servers:
  one:
    address: "{addr}"
health:
  method: status
  interval: 20ms
  timeout: 500ms
  fall: 1
  rise: 1
  start_healthy: false
"#
        );
        let config = Config::parse(&yaml, "test").unwrap().config;
        let registry = Registry::build(Arc::new(config));
        let backend = Arc::clone(registry.backend("one").unwrap());
        assert!(!backend.is_healthy(), "start_healthy: false means unproven");

        let (tx, rx) = watch::channel(false);
        let checker = tokio::spawn(run(Arc::clone(&registry), rx));

        for _ in 0..100 {
            if backend.is_healthy() {
                break;
            }
            time::sleep(std::time::Duration::from_millis(20)).await;
        }

        assert!(backend.is_healthy());
        let status = backend.status().expect("a status check records a snapshot");
        assert_eq!(status.online, 12);
        assert_eq!(registry.reported_players(), 12);

        tx.send(true).unwrap();
        checker.await.unwrap();
    }
}
