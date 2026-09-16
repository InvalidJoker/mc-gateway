//! The backend registry: what exists, what is healthy, and which one gets the
//! next player.

use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use mc_config::{BackendKind, Config, Forwarding, Health, Policy, Rule};

use crate::matcher::Matcher;

/// A backend server plus everything the gateway learned about it at runtime.
#[derive(Debug)]
pub struct Backend {
    pub name: String,
    /// `host:port`, resolved per connect.
    pub address: String,
    pub kind: BackendKind,
    pub forwarding: Forwarding,
    pub weight: u32,
    /// 0 means unlimited.
    pub max_connections: usize,
    pub health_config: Health,
    pub group: Option<String>,

    healthy: AtomicBool,
    consecutive_ok: AtomicU32,
    consecutive_fail: AtomicU32,
    active: AtomicUsize,
    sessions_total: AtomicU64,
    status: RwLock<Option<StatusSnapshot>>,
}

/// What the last successful status health check saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub online: i64,
    pub max: i64,
    pub version: String,
    pub protocol: i32,
    pub latency: Duration,
}

impl Backend {
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn active_sessions(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub fn sessions_total(&self) -> u64 {
        self.sessions_total.load(Ordering::Relaxed)
    }

    pub fn status(&self) -> Option<StatusSnapshot> {
        self.status.read().expect("status lock is never poisoned").clone()
    }

    pub fn is_full(&self) -> bool {
        self.max_connections != 0 && self.active_sessions() >= self.max_connections
    }

    /// Claims a session slot, respecting `max_connections`.
    ///
    /// The returned guard releases the slot on drop, so every early return in
    /// the session code path stays accounted for.
    pub fn try_acquire(self: &Arc<Self>) -> Option<SessionGuard> {
        if self.max_connections == 0 {
            self.active.fetch_add(1, Ordering::Relaxed);
        } else {
            // Compare-and-swap so two connections cannot both pass the limit.
            let mut current = self.active.load(Ordering::Relaxed);
            loop {
                if current >= self.max_connections {
                    return None;
                }
                match self.active.compare_exchange_weak(
                    current,
                    current + 1,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
        }
        self.sessions_total.fetch_add(1, Ordering::Relaxed);
        Some(SessionGuard { backend: Arc::clone(self) })
    }

    /// Feeds one health check result into the rise/fall state machine.
    ///
    /// Returns `Some(new_state)` when the backend actually changed state, so
    /// the caller logs a transition rather than every single probe.
    pub fn record_health(&self, ok: bool, snapshot: Option<StatusSnapshot>) -> Option<bool> {
        if let Some(snapshot) = snapshot {
            *self.status.write().expect("status lock is never poisoned") = Some(snapshot);
        }

        if ok {
            self.consecutive_fail.store(0, Ordering::Relaxed);
            let streak = self.consecutive_ok.fetch_add(1, Ordering::Relaxed) + 1;
            if !self.is_healthy() && streak >= self.health_config.rise.max(1) {
                self.healthy.store(true, Ordering::Relaxed);
                return Some(true);
            }
        } else {
            self.consecutive_ok.store(0, Ordering::Relaxed);
            let streak = self.consecutive_fail.fetch_add(1, Ordering::Relaxed) + 1;
            if self.is_healthy() && streak >= self.health_config.fall.max(1) {
                self.healthy.store(false, Ordering::Relaxed);
                *self.status.write().expect("status lock is never poisoned") = None;
                return Some(false);
            }
        }
        None
    }

    /// Carries runtime state across a config reload for a server that did not
    /// change identity, so a reload does not blank the health of 300 servers.
    pub fn inherit_from(&self, previous: &Backend) {
        self.healthy.store(previous.is_healthy(), Ordering::Relaxed);
        self.active.store(previous.active_sessions(), Ordering::Relaxed);
        self.sessions_total.store(previous.sessions_total(), Ordering::Relaxed);
        *self.status.write().expect("status lock is never poisoned") = previous.status();
    }
}

/// Holds a backend's session slot for as long as the session lives.
#[derive(Debug)]
pub struct SessionGuard {
    backend: Arc<Backend>,
}

impl SessionGuard {
    pub fn backend(&self) -> &Arc<Backend> {
        &self.backend
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.backend.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A routing target: either one server, or a group of them.
#[derive(Debug)]
pub struct Target {
    pub name: String,
    pub policy: Policy,
    members: Vec<Arc<Backend>>,
    /// Weighted round-robin wheel of indices into `members`.
    wheel: Vec<usize>,
    cursor: AtomicUsize,
}

/// Total wheel size, so a typo like `weight: 100000` cannot allocate wildly.
const MAX_WHEEL: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SelectError {
    #[error("no such target")]
    UnknownTarget,
    #[error("target has no members")]
    Empty,
    #[error("every backend is unhealthy")]
    AllUnhealthy,
    #[error("every backend is at its connection limit")]
    AllFull,
}

impl Target {
    pub fn members(&self) -> &[Arc<Backend>] {
        &self.members
    }

    pub fn healthy_members(&self) -> impl Iterator<Item = &Arc<Backend>> {
        self.members.iter().filter(|b| b.is_healthy())
    }

    pub fn active_sessions(&self) -> usize {
        self.members.iter().map(|b| b.active_sessions()).sum()
    }

    /// Picks the backend for the next session.
    pub fn select(&self) -> Result<Arc<Backend>, SelectError> {
        if self.members.is_empty() {
            return Err(SelectError::Empty);
        }
        let any_healthy = self.members.iter().any(|b| b.is_healthy());
        if !any_healthy {
            return Err(SelectError::AllUnhealthy);
        }

        let picked = match self.policy {
            Policy::RoundRobin => self.select_round_robin(),
            Policy::LeastConnections => self
                .members
                .iter()
                .filter(|b| b.is_healthy() && !b.is_full())
                .min_by_key(|b| b.active_sessions())
                .cloned(),
            Policy::Failover => {
                self.members.iter().find(|b| b.is_healthy() && !b.is_full()).cloned()
            }
        };

        picked.ok_or(SelectError::AllFull)
    }

    fn select_round_robin(&self) -> Option<Arc<Backend>> {
        if self.wheel.is_empty() {
            return None;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        // One full pass: if nothing is selectable, the caller gets AllFull.
        (0..self.wheel.len()).find_map(|offset| {
            let slot = self.wheel[(start.wrapping_add(offset)) % self.wheel.len()];
            let backend = &self.members[slot];
            (backend.is_healthy() && !backend.is_full()).then(|| Arc::clone(backend))
        })
    }
}

/// Immutable snapshot of routing state. A config reload builds a new one and
/// swaps it in; in-flight sessions keep using the old one until they end.
#[derive(Debug)]
pub struct Registry {
    config: Arc<Config>,
    backends: HashMap<String, Arc<Backend>>,
    targets: HashMap<String, Arc<Target>>,
    matcher: Matcher,
}

/// A matched route, ready to be turned into a connection.
#[derive(Debug, Clone)]
pub struct Route<'a> {
    pub target: Arc<Target>,
    pub rule: Option<&'a Rule>,
}

impl Registry {
    pub fn build(config: Arc<Config>) -> Arc<Self> {
        Self::build_inheriting(config, None)
    }

    /// Builds a registry, carrying health and session counts over from a
    /// previous one where the server definition is unchanged.
    pub fn build_inheriting(config: Arc<Config>, previous: Option<&Registry>) -> Arc<Self> {
        let mut backends = HashMap::with_capacity(config.servers.len());

        for (name, server) in &config.servers {
            let health_config = config.health.overlay(server.health.as_ref());
            let backend = Arc::new(Backend {
                name: name.clone(),
                address: server.address.clone(),
                kind: server.kind,
                forwarding: server.forwarding,
                weight: server.weight,
                max_connections: server.max_connections,
                group: server.group.clone(),
                healthy: AtomicBool::new(
                    !health_config.enabled || config.health.start_healthy,
                ),
                consecutive_ok: AtomicU32::new(0),
                consecutive_fail: AtomicU32::new(0),
                active: AtomicUsize::new(0),
                sessions_total: AtomicU64::new(0),
                status: RwLock::new(None),
                health_config,
            });

            if let Some(old) = previous.and_then(|p| p.backends.get(name))
                && old.address == backend.address
            {
                backend.inherit_from(old);
            }

            backends.insert(name.clone(), backend);
        }

        let mut targets: HashMap<String, Arc<Target>> = HashMap::new();
        for name in config.target_names() {
            let members: Vec<Arc<Backend>> = config
                .resolve_target(name)
                .into_iter()
                .filter_map(|server| backends.get(server).cloned())
                .collect();
            let policy = config.groups.get(name).map(|g| g.policy).unwrap_or_default();
            targets.insert(name.to_owned(), Arc::new(build_target(name.to_owned(), policy, members)));
        }

        let matcher = Matcher::build(&config);
        Arc::new(Self { config, backends, targets, matcher })
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.config
    }

    /// Resolves a handshake hostname to a target.
    pub fn route(&self, host: &str, listener: &str) -> Option<Route<'_>> {
        let hit = self.matcher.match_host(host, listener)?;
        let target = self.targets.get(hit.target)?;
        Some(Route { target: Arc::clone(target), rule: hit.rule })
    }

    pub fn target(&self, name: &str) -> Option<&Arc<Target>> {
        self.targets.get(name)
    }

    pub fn backend(&self, name: &str) -> Option<&Arc<Backend>> {
        self.backends.get(name)
    }

    pub fn backends(&self) -> impl ExactSizeIterator<Item = &Arc<Backend>> {
        self.backends.values()
    }

    pub fn active_sessions(&self) -> usize {
        self.backends.values().map(|b| b.active_sessions()).sum()
    }

    pub fn healthy_count(&self) -> usize {
        self.backends.values().filter(|b| b.is_healthy()).count()
    }

    /// Players reported by the backends themselves, for `motd.players: backends`.
    pub fn reported_players(&self) -> i64 {
        self.backends.values().filter_map(|b| b.status()).map(|s| s.online).sum()
    }
}

fn build_target(name: String, policy: Policy, members: Vec<Arc<Backend>>) -> Target {
    let total_weight: u32 = members.iter().map(|b| b.weight).sum();
    let scale = if total_weight as usize > MAX_WHEEL {
        (total_weight as usize).div_ceil(MAX_WHEEL)
    } else {
        1
    };

    let mut wheel = Vec::new();
    for (index, backend) in members.iter().enumerate() {
        let slots = (backend.weight as usize / scale).max(usize::from(backend.weight > 0));
        wheel.extend(std::iter::repeat_n(index, slots));
    }

    Target { name, policy, members, wheel, cursor: AtomicUsize::new(0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(yaml: &str) -> Arc<Registry> {
        let config = Config::parse(yaml, "test").unwrap().config;
        Registry::build(Arc::new(config))
    }

    const GROUP: &str = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  default: survival
servers:
  survival-01:
    address: "10.0.0.1:25565"
    group: survival
  survival-02:
    address: "10.0.0.2:25565"
    group: survival
groups:
  survival:
    policy: POLICY
health:
  enabled: false
  fall: 1
  rise: 1
"#;

    fn group_registry(policy: &str) -> Arc<Registry> {
        registry(&GROUP.replace("POLICY", policy))
    }

    #[test]
    fn round_robin_alternates_between_members() {
        let registry = group_registry("round_robin");
        let target = registry.target("survival").unwrap();
        let picks: Vec<String> =
            (0..4).map(|_| target.select().unwrap().name.clone()).collect();
        assert_eq!(picks[0], picks[2], "the wheel repeats after one lap");
        assert_ne!(picks[0], picks[1], "consecutive picks differ");
    }

    #[test]
    fn round_robin_skips_unhealthy_members() {
        let registry = group_registry("round_robin");
        let down = registry.backend("survival-01").unwrap();
        down.record_health(false, None);
        assert!(!down.is_healthy());

        let target = registry.target("survival").unwrap();
        for _ in 0..6 {
            assert_eq!(target.select().unwrap().name, "survival-02");
        }
    }

    #[test]
    fn least_connections_prefers_the_quieter_server() {
        let registry = group_registry("least_connections");
        let busy = registry.backend("survival-01").unwrap();
        let _guards: Vec<_> = (0..3).map(|_| busy.try_acquire().unwrap()).collect();

        let target = registry.target("survival").unwrap();
        assert_eq!(target.select().unwrap().name, "survival-02");
        assert_eq!(busy.active_sessions(), 3);
    }

    #[test]
    fn failover_stays_on_the_first_healthy_member() {
        let registry = group_registry("failover");
        let target = registry.target("survival").unwrap();
        for _ in 0..3 {
            assert_eq!(target.select().unwrap().name, "survival-01");
        }
        registry.backend("survival-01").unwrap().record_health(false, None);
        assert_eq!(target.select().unwrap().name, "survival-02");
    }

    #[test]
    fn selection_reports_why_it_failed() {
        let registry = group_registry("round_robin");
        let target = registry.target("survival").unwrap();
        for backend in target.members() {
            backend.record_health(false, None);
        }
        assert_eq!(target.select().unwrap_err(), SelectError::AllUnhealthy);
    }

    #[test]
    fn connection_limits_are_enforced_and_released() {
        let yaml = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  default: small
servers:
  small:
    address: "10.0.0.1:25565"
    max_connections: 2
health:
  enabled: false
"#;
        let registry = registry(yaml);
        let backend = registry.backend("small").unwrap();
        let first = backend.try_acquire().unwrap();
        let second = backend.try_acquire().unwrap();
        assert!(backend.try_acquire().is_none(), "the limit holds");
        assert!(backend.is_full());

        drop(first);
        assert_eq!(backend.active_sessions(), 1);
        let third = backend.try_acquire();
        assert!(third.is_some(), "a freed slot is reusable");
        drop((second, third));
        assert_eq!(backend.active_sessions(), 0);
        // The rejected third attempt never took a slot, so it never counted.
        assert_eq!(backend.sessions_total(), 3, "totals keep counting");
    }

    #[test]
    fn a_full_target_is_distinguished_from_an_unhealthy_one() {
        let yaml = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  default: small
servers:
  small:
    address: "10.0.0.1:25565"
    max_connections: 1
health:
  enabled: false
"#;
        let registry = registry(yaml);
        let _guard = registry.backend("small").unwrap().try_acquire().unwrap();
        assert_eq!(registry.target("small").unwrap().select().unwrap_err(), SelectError::AllFull);
    }

    #[test]
    fn rise_and_fall_thresholds_debounce_flapping() {
        let yaml = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  default: one
servers:
  one:
    address: "10.0.0.1:25565"
health:
  rise: 2
  fall: 3
"#;
        let registry = registry(yaml);
        let backend = registry.backend("one").unwrap();
        assert!(backend.is_healthy(), "start_healthy avoids a cold-start outage");

        assert_eq!(backend.record_health(false, None), None);
        assert_eq!(backend.record_health(false, None), None);
        assert!(backend.is_healthy(), "two failures are not enough");
        assert_eq!(backend.record_health(false, None), Some(false));
        assert!(!backend.is_healthy());

        assert_eq!(backend.record_health(true, None), None);
        assert_eq!(backend.record_health(true, None), Some(true));
        assert!(backend.is_healthy());
    }

    #[test]
    fn weights_bias_the_wheel() {
        let yaml = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  default: pool
servers:
  big:
    address: "10.0.0.1:25565"
    group: pool
    weight: 3
  small:
    address: "10.0.0.2:25565"
    group: pool
    weight: 1
health:
  enabled: false
"#;
        let registry = registry(yaml);
        let target = registry.target("pool").unwrap();
        let mut big = 0;
        for _ in 0..40 {
            if target.select().unwrap().name == "big" {
                big += 1;
            }
        }
        assert_eq!(big, 30, "3:1 weighting over 40 picks");
    }

    #[test]
    fn a_reload_keeps_health_and_session_counts() {
        let registry = group_registry("round_robin");
        let backend = registry.backend("survival-01").unwrap();
        backend.record_health(false, None);
        let _guard = registry.backend("survival-02").unwrap().try_acquire().unwrap();

        let config = Config::parse(&GROUP.replace("POLICY", "failover"), "test").unwrap().config;
        let reloaded = Registry::build_inheriting(Arc::new(config), Some(&registry));

        assert!(!reloaded.backend("survival-01").unwrap().is_healthy(), "health carries over");
        assert_eq!(reloaded.backend("survival-02").unwrap().active_sessions(), 1);
        assert_eq!(reloaded.target("survival").unwrap().policy, mc_config::Policy::Failover);
    }

    #[test]
    fn a_moved_backend_restarts_from_a_clean_slate() {
        let registry = group_registry("round_robin");
        registry.backend("survival-01").unwrap().record_health(false, None);

        let moved = GROUP.replace("POLICY", "round_robin").replace("10.0.0.1:25565", "10.9.9.9:25565");
        let config = Config::parse(&moved, "test").unwrap().config;
        let reloaded = Registry::build_inheriting(Arc::new(config), Some(&registry));

        assert!(
            reloaded.backend("survival-01").unwrap().is_healthy(),
            "a new address is a new server, not the old one's health"
        );
    }
}
