//! Metrics, in the Prometheus text format, with no HTTP framework behind them.
//!
//! The exporter is roughly a hundred lines of tokio: a scrape endpoint has one
//! route, no body parsing and no auth, and pulling in a web stack for that
//! would be a larger maintenance surface than the gateway's own protocol code.

pub mod exporter;

use std::{
    collections::HashMap,
    fmt::Write,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

/// A counter split by one label value.
#[derive(Debug, Default)]
pub struct LabelCounter {
    values: RwLock<HashMap<Box<str>, Arc<AtomicU64>>>,
}

impl LabelCounter {
    pub fn increment(&self, label: &str) {
        self.add(label, 1);
    }

    pub fn add(&self, label: &str, amount: u64) {
        // Fast path: the label already exists, so a read lock is enough.
        if let Some(counter) = self.values.read().expect("counter lock").get(label) {
            counter.fetch_add(amount, Ordering::Relaxed);
            return;
        }
        let mut values = self.values.write().expect("counter lock");
        values
            .entry(label.into())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .fetch_add(amount, Ordering::Relaxed);
    }

    pub fn get(&self, label: &str) -> u64 {
        self.values
            .read()
            .expect("counter lock")
            .get(label)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let mut out: Vec<(String, u64)> = self
            .values
            .read()
            .expect("counter lock")
            .iter()
            .map(|(k, v)| (k.to_string(), v.load(Ordering::Relaxed)))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

/// Everything the gateway counts.
///
/// Gauges that can be derived from live state (active sessions, backend health)
/// are *not* stored here; they are read from the registry at scrape time by a
/// [`Collector`], so they can never drift out of sync with reality.
#[derive(Debug, Default)]
pub struct Metrics {
    pub connections_total: AtomicU64,
    pub connections_active: AtomicU64,
    /// `reason` = rate_limited | per_ip_limit | global_limit | no_route |
    /// no_backend | backend_error | handshake_error | proxy_protocol
    pub connections_rejected: LabelCounter,
    /// `kind` from the protocol parser.
    pub handshake_errors: LabelCounter,
    /// `source` = gateway | backend | legacy
    pub status_requests: LabelCounter,
    pub login_attempts: AtomicU64,
    /// `backend` name.
    pub backend_connections: LabelCounter,
    /// `backend` name; the failure kind is folded in as `name:kind`.
    pub backend_failures: LabelCounter,
    /// `direction` = to_backend | to_client
    pub bytes: LabelCounter,
    pub sessions_completed: AtomicU64,
    pub session_seconds_total: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn connection_opened(&self) {
        self.connections_total.fetch_add(1, Ordering::Relaxed);
        self.connections_active.fetch_add(1, Ordering::Relaxed);
    }

    pub fn connection_closed(&self) {
        self.connections_active.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn session_finished(&self, seconds: u64) {
        self.sessions_completed.fetch_add(1, Ordering::Relaxed);
        self.session_seconds_total.fetch_add(seconds, Ordering::Relaxed);
    }

    /// Renders the whole registry in the Prometheus text exposition format.
    pub fn render(&self, extra: Option<&dyn Collector>) -> String {
        let mut out = String::with_capacity(4096);

        counter(&mut out, "mc_gateway_connections_total", "Client connections accepted",
            self.connections_total.load(Ordering::Relaxed));
        gauge(&mut out, "mc_gateway_connections_active", "Client connections currently open",
            self.connections_active.load(Ordering::Relaxed) as f64);
        labelled(&mut out, "mc_gateway_connections_rejected_total",
            "Connections refused before reaching a backend", "reason",
            &self.connections_rejected.snapshot(), "counter");
        labelled(&mut out, "mc_gateway_handshake_errors_total",
            "Handshakes that could not be parsed", "kind",
            &self.handshake_errors.snapshot(), "counter");
        labelled(&mut out, "mc_gateway_status_requests_total",
            "Server list pings answered", "source",
            &self.status_requests.snapshot(), "counter");
        counter(&mut out, "mc_gateway_login_attempts_total", "Handshakes in the login state",
            self.login_attempts.load(Ordering::Relaxed));
        labelled(&mut out, "mc_gateway_backend_connections_total",
            "Sessions opened towards each backend", "backend",
            &self.backend_connections.snapshot(), "counter");
        labelled(&mut out, "mc_gateway_backend_failures_total",
            "Failed attempts to reach a backend", "backend",
            &self.backend_failures.snapshot(), "counter");
        labelled(&mut out, "mc_gateway_bytes_total", "Bytes proxied", "direction",
            &self.bytes.snapshot(), "counter");
        counter(&mut out, "mc_gateway_sessions_completed_total", "Proxied sessions that ended",
            self.sessions_completed.load(Ordering::Relaxed));
        counter(&mut out, "mc_gateway_session_seconds_total",
            "Summed duration of completed sessions",
            self.session_seconds_total.load(Ordering::Relaxed));

        if let Some(extra) = extra {
            extra.collect(&mut out);
        }

        out
    }
}

/// Supplies gauges that are read from live state at scrape time.
pub trait Collector: Send + Sync {
    fn collect(&self, out: &mut String);
}

pub fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}");
}

pub fn gauge(out: &mut String, name: &str, help: &str, value: f64) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {}", format_float(value));
}

/// Writes a metric family with one label.
pub fn labelled(
    out: &mut String,
    name: &str,
    help: &str,
    label: &str,
    values: &[(String, u64)],
    kind: &str,
) {
    if values.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    for (value_label, value) in values {
        let _ = writeln!(out, "{name}{{{label}=\"{}\"}} {value}", escape(value_label));
    }
}

/// Header for a family whose samples are written by the caller.
pub fn family_header(out: &mut String, name: &str, help: &str, kind: &str) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
}

pub fn sample(out: &mut String, name: &str, labels: &[(&str, &str)], value: f64) {
    let rendered: Vec<String> =
        labels.iter().map(|(k, v)| format!("{k}=\"{}\"", escape(v))).collect();
    let _ = writeln!(out, "{name}{{{}}} {}", rendered.join(","), format_float(value));
}

/// Prometheus label values may not contain a raw backslash, quote or newline.
pub fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

fn format_float(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters_and_gauges() {
        let metrics = Metrics::new();
        metrics.connection_opened();
        metrics.connection_opened();
        metrics.connection_closed();
        metrics.connections_rejected.increment("rate_limited");
        metrics.bytes.add("to_backend", 4096);

        let text = metrics.render(None);
        assert!(text.contains("mc_gateway_connections_total 2"), "{text}");
        assert!(text.contains("mc_gateway_connections_active 1"), "{text}");
        assert!(text.contains(r#"mc_gateway_connections_rejected_total{reason="rate_limited"} 1"#));
        assert!(text.contains(r#"mc_gateway_bytes_total{direction="to_backend"} 4096"#));
        assert!(text.contains("# TYPE mc_gateway_connections_total counter"));
    }

    #[test]
    fn empty_families_are_omitted_entirely() {
        let text = Metrics::new().render(None);
        assert!(!text.contains("mc_gateway_backend_connections_total"), "{text}");
    }

    #[test]
    fn label_values_are_escaped() {
        let metrics = Metrics::new();
        metrics.backend_failures.increment(r#"weird"name\with"#);
        let text = metrics.render(None);
        assert!(text.contains(r#"backend="weird\"name\\with""#), "{text}");
    }

    #[test]
    fn counters_are_stable_under_concurrent_updates() {
        let metrics = Metrics::new();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    for _ in 0..1000 {
                        metrics.connection_opened();
                        metrics.backend_connections.increment("survival-01");
                    }
                });
            }
        });
        assert_eq!(metrics.connections_total.load(Ordering::Relaxed), 8000);
        assert_eq!(metrics.backend_connections.get("survival-01"), 8000);
    }

    #[test]
    fn extra_collectors_are_appended() {
        struct Live;
        impl Collector for Live {
            fn collect(&self, out: &mut String) {
                family_header(out, "mc_gateway_backend_up", "Backend health", "gauge");
                sample(out, "mc_gateway_backend_up", &[("backend", "survival-01")], 1.0);
            }
        }
        let text = Metrics::new().render(Some(&Live));
        assert!(text.contains(r#"mc_gateway_backend_up{backend="survival-01"} 1"#), "{text}");
    }
}
