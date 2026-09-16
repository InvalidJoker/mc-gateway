//! Metrics.
//!
//! The registry, the Prometheus text format and the scrape endpoint all come
//! from [`metrics`] and [`metrics_exporter_prometheus`]. What is left here is
//! the part that is actually about this gateway: which metrics exist, what they
//! mean, and where the numbers come from.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use arc_swap::ArcSwap;
use metrics::{counter, describe_counter, describe_gauge, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::{sync::watch, time};
use tracing::info;

use crate::app::Runtime;

/// How often live gauges are refreshed from the registry.
const GAUGE_INTERVAL: Duration = Duration::from_secs(5);

/// Starts the scrape endpoint and documents every metric.
///
/// Must be called from inside the tokio runtime: the exporter spawns its own
/// listener task.
pub fn install(bind: SocketAddr) -> Result<(), String> {
    PrometheusBuilder::new()
        .with_http_listener(bind)
        .install()
        .map_err(|err| format!("cannot start the metrics endpoint on {bind}: {err}"))?;

    describe();
    info!(%bind, "metrics endpoint listening");
    Ok(())
}

fn describe() {
    describe_counter!("mc_gateway_connections_total", "Client connections accepted");
    describe_counter!(
        "mc_gateway_connections_rejected_total",
        "Connections refused before reaching a backend"
    );
    describe_counter!("mc_gateway_handshake_errors_total", "Handshakes that could not be parsed");
    describe_counter!("mc_gateway_status_requests_total", "Server list pings proxied");
    describe_counter!("mc_gateway_login_attempts_total", "Handshakes in the login state");
    describe_counter!("mc_gateway_backend_connections_total", "Connections opened to a backend");
    describe_counter!("mc_gateway_backend_failures_total", "Failed attempts to reach a backend");
    describe_counter!("mc_gateway_bytes_total", "Bytes proxied");
    describe_counter!("mc_gateway_sessions_completed_total", "Proxied sessions that ended");
    describe_counter!(
        "mc_gateway_sessions_aborted_total",
        "Sessions ended by an error or timeout, whose byte counts are unknown"
    );
    describe_counter!("mc_gateway_motd_rewrites_total", "Status responses whose MOTD was rewritten");

    describe_gauge!("mc_gateway_connections_active", "Client connections currently open");
    describe_gauge!("mc_gateway_sessions_active", "Sessions currently proxied to a backend");
    describe_gauge!("mc_gateway_backends", "Configured backends");
    describe_gauge!("mc_gateway_backends_healthy", "Backends passing health checks");
    describe_gauge!("mc_gateway_backend_up", "1 when the backend is healthy");
    describe_gauge!("mc_gateway_backend_sessions", "Sessions currently proxied to this backend");
    describe_gauge!(
        "mc_gateway_backend_players",
        "Players reported by the backend's own status ping"
    );
    describe_gauge!(
        "mc_gateway_backend_ping_seconds",
        "Round trip of the last status health check"
    );
}

pub fn connection_opened() {
    counter!("mc_gateway_connections_total").increment(1);
    gauge!("mc_gateway_connections_active").increment(1.0);
}

pub fn connection_closed() {
    gauge!("mc_gateway_connections_active").decrement(1.0);
}

pub fn connection_rejected(reason: &'static str) {
    counter!("mc_gateway_connections_rejected_total", "reason" => reason).increment(1);
}

pub fn handshake_error(kind: &'static str) {
    counter!("mc_gateway_handshake_errors_total", "kind" => kind).increment(1);
}

/// `kind` is `status` or `legacy`.
pub fn status_request(kind: &'static str) {
    counter!("mc_gateway_status_requests_total", "kind" => kind).increment(1);
}

pub fn motd_rewritten() {
    counter!("mc_gateway_motd_rewrites_total").increment(1);
}

pub fn login_attempt() {
    counter!("mc_gateway_login_attempts_total").increment(1);
}

pub fn backend_connected(backend: &str) {
    counter!("mc_gateway_backend_connections_total", "backend" => backend.to_owned()).increment(1);
}

pub fn backend_failed(backend: &str, kind: &'static str) {
    counter!(
        "mc_gateway_backend_failures_total",
        "backend" => backend.to_owned(),
        "kind" => kind,
    )
    .increment(1);
}

pub fn session_started(backend: &str) {
    gauge!("mc_gateway_sessions_active").increment(1.0);
    gauge!("mc_gateway_backend_sessions", "backend" => backend.to_owned()).increment(1.0);
}

pub fn session_ended(backend: &str, bytes: Option<(u64, u64)>) {
    gauge!("mc_gateway_sessions_active").decrement(1.0);
    gauge!("mc_gateway_backend_sessions", "backend" => backend.to_owned()).decrement(1.0);
    counter!("mc_gateway_sessions_completed_total").increment(1);

    match bytes {
        Some((to_backend, to_client)) => {
            counter!("mc_gateway_bytes_total", "direction" => "to_backend").increment(to_backend);
            counter!("mc_gateway_bytes_total", "direction" => "to_client").increment(to_client);
        }
        // `copy_bidirectional` reports byte counts only on a clean close, so a
        // session torn down by a timeout or a reset contributes to this counter
        // instead of to the byte totals.
        None => counter!("mc_gateway_sessions_aborted_total").increment(1),
    }
}

/// Keeps registry-derived gauges fresh.
///
/// These are pulled on a timer rather than pushed from the health checker, so
/// `mc-routing` stays free of any metrics dependency.
pub async fn run_gauges(runtime: Arc<ArcSwap<Runtime>>, mut shutdown: watch::Receiver<bool>) {
    let mut ticker = time::interval(GAUGE_INTERVAL);
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => sample_registry(&runtime),
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
            }
        }
    }
}

fn sample_registry(runtime: &ArcSwap<Runtime>) {
    let registry = &runtime.load().registry;

    gauge!("mc_gateway_backends").set(registry.backends().len() as f64);
    gauge!("mc_gateway_backends_healthy").set(registry.healthy_count() as f64);

    for backend in registry.backends() {
        let name = backend.name.clone();
        gauge!(
            "mc_gateway_backend_up",
            "backend" => name.clone(),
            "kind" => backend.kind.as_str(),
        )
        .set(f64::from(u8::from(backend.is_healthy())));

        if let Some(status) = backend.status() {
            gauge!("mc_gateway_backend_players", "backend" => name.clone())
                .set(status.online as f64);
            gauge!("mc_gateway_backend_ping_seconds", "backend" => name)
                .set(status.latency.as_secs_f64());
        }
    }
}
