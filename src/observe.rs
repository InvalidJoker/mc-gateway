//! Metrics, served for Prometheus by `metrics-exporter-prometheus`.

use std::net::SocketAddr;

use metrics::{counter, describe_counter, describe_gauge, gauge};
use metrics_exporter_prometheus::PrometheusBuilder;

/// Starts the scrape endpoint. Must run inside the tokio runtime.
pub fn install(bind: SocketAddr) -> Result<(), String> {
    PrometheusBuilder::new()
        .with_http_listener(bind)
        .install()
        .map_err(|err| format!("cannot start the metrics endpoint on {bind}: {err}"))?;

    describe_counter!("mc_gateway_connections_total", "Intercepted connections");
    describe_gauge!("mc_gateway_connections_active", "Intercepted connections currently open");
    describe_counter!("mc_gateway_status_requests_total", "Status pings recognised");
    describe_counter!("mc_gateway_motd_rewrites_total", "Status responses that got the MOTD line");
    describe_counter!("mc_gateway_server_unreachable_total", "Connections whose server did not answer");
    Ok(())
}

pub fn connection_opened() {
    counter!("mc_gateway_connections_total").increment(1);
    gauge!("mc_gateway_connections_active").increment(1.0);
}

pub fn connection_closed() {
    gauge!("mc_gateway_connections_active").decrement(1.0);
}

pub fn status_request() {
    counter!("mc_gateway_status_requests_total").increment(1);
}

pub fn motd_rewritten() {
    counter!("mc_gateway_motd_rewrites_total").increment(1);
}

pub fn server_unreachable() {
    counter!("mc_gateway_server_unreachable_total").increment(1);
}
