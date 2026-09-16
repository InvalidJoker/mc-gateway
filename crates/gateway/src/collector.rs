//! Gauges read from live state at scrape time.
//!
//! Health and session counts are not mirrored into counters, because a mirror
//! can drift. The registry is the single source of truth and is read directly.

use std::sync::Arc;

use arc_swap::ArcSwap;
use mc_metrics::{Collector, family_header, gauge, sample};

use crate::app::Runtime;

pub struct RegistryCollector {
    runtime: Arc<ArcSwap<Runtime>>,
}

impl RegistryCollector {
    pub fn new(runtime: Arc<ArcSwap<Runtime>>) -> Arc<Self> {
        Arc::new(Self { runtime })
    }
}

impl Collector for RegistryCollector {
    fn collect(&self, out: &mut String) {
        let runtime = self.runtime.load();
        let registry = &runtime.registry;

        gauge(out, "mc_gateway_backends", "Configured backends", registry.backends().len() as f64);
        gauge(
            out,
            "mc_gateway_backends_healthy",
            "Backends passing health checks",
            registry.healthy_count() as f64,
        );
        gauge(
            out,
            "mc_gateway_sessions_active",
            "Sessions currently proxied to a backend",
            registry.active_sessions() as f64,
        );

        family_header(out, "mc_gateway_backend_up", "1 when the backend is healthy", "gauge");
        let mut backends: Vec<_> = registry.backends().collect();
        backends.sort_by(|a, b| a.name.cmp(&b.name));
        for backend in &backends {
            sample(
                out,
                "mc_gateway_backend_up",
                &[("backend", &backend.name), ("kind", backend.kind.as_str())],
                f64::from(u8::from(backend.is_healthy())),
            );
        }

        family_header(
            out,
            "mc_gateway_backend_sessions",
            "Sessions currently proxied to this backend",
            "gauge",
        );
        for backend in &backends {
            sample(
                out,
                "mc_gateway_backend_sessions",
                &[("backend", &backend.name)],
                backend.active_sessions() as f64,
            );
        }

        let reported: Vec<_> =
            backends.iter().filter_map(|b| b.status().map(|s| (&b.name, s))).collect();
        if !reported.is_empty() {
            family_header(
                out,
                "mc_gateway_backend_players",
                "Players reported by the backend's own status ping",
                "gauge",
            );
            for (name, status) in &reported {
                sample(out, "mc_gateway_backend_players", &[("backend", name)], status.online as f64);
            }

            family_header(
                out,
                "mc_gateway_backend_ping_seconds",
                "Round trip of the last status health check",
                "gauge",
            );
            for (name, status) in &reported {
                sample(
                    out,
                    "mc_gateway_backend_ping_seconds",
                    &[("backend", name)],
                    status.latency.as_secs_f64(),
                );
            }
        }
    }
}
