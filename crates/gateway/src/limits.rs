//! Admission control.
//!
//! Everything here runs before a single Minecraft byte is read, because that is
//! the only place where the cost of an abusive connection is still near zero.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use arc_swap::ArcSwap;
use mc_config::{Limits, cidr};

/// Why a connection was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The gateway is at `limits.max_connections`.
    GlobalLimit,
    /// This IP is at `limits.max_connections_per_ip`.
    PerIpLimit,
    /// This IP emptied its connection-rate bucket.
    RateLimited,
}

impl Rejection {
    /// Metrics label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Rejection::GlobalLimit => "global_limit",
            Rejection::PerIpLimit => "per_ip_limit",
            Rejection::RateLimited => "rate_limited",
        }
    }
}

/// Per-IP accounting. Entries are dropped once an IP is idle and its bucket has
/// refilled, so the map tracks active abusers rather than every visitor ever.
#[derive(Debug)]
struct IpState {
    active: usize,
    tokens: f64,
    last_refill: Instant,
}

#[derive(Debug)]
pub struct Limiter {
    /// Swappable so a config reload can tighten limits without dropping the
    /// per-IP state that is currently holding abusers back.
    limits: ArcSwap<Limits>,
    active: AtomicUsize,
    per_ip: Mutex<HashMap<IpAddr, IpState>>,
}

/// Number of tracked IPs above which idle entries are swept.
const SWEEP_THRESHOLD: usize = 4096;

impl Limiter {
    pub fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            limits: ArcSwap::from_pointee(limits),
            active: AtomicUsize::new(0),
            per_ip: Mutex::new(HashMap::new()),
        })
    }

    /// Applies new limits from a config reload.
    pub fn set_limits(&self, limits: Limits) {
        self.limits.store(Arc::new(limits));
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub fn tracked_ips(&self) -> usize {
        self.per_ip.lock().expect("limiter lock").len()
    }

    /// Admits a connection, or explains why not.
    ///
    /// The returned permit releases both counters on drop, so no error path can
    /// leak a slot.
    pub fn admit(self: &Arc<Self>, ip: IpAddr) -> Result<Permit, Rejection> {
        // Normalise `::ffff:1.2.3.4` so a dual-stack listener does not give the
        // same client two separate budgets.
        let ip = cidr::unmap(ip);
        let limits = self.limits.load();

        let global = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        // Built before any early return so every rejection path still releases
        // the global slot when it drops.
        let mut permit = Permit { limiter: Arc::clone(self), ip, counted_per_ip: false };
        if limits.max_connections != 0 && global > limits.max_connections {
            return Err(Rejection::GlobalLimit);
        }

        if limits.exempt.contains(ip) {
            return Ok(permit);
        }

        let mut table = self.per_ip.lock().expect("limiter lock");
        if table.len() >= SWEEP_THRESHOLD {
            sweep(&mut table, limits.connection_rate.per.get());
        }

        let now = Instant::now();
        let burst = f64::from(limits.connection_rate.burst);
        let entry = table.entry(ip).or_insert_with(|| IpState {
            active: 0,
            tokens: burst,
            last_refill: now,
        });

        if limits.max_connections_per_ip != 0 && entry.active >= limits.max_connections_per_ip {
            return Err(Rejection::PerIpLimit);
        }

        if !limits.connection_rate.is_disabled() {
            let window = limits.connection_rate.per.get().as_secs_f64();
            let elapsed = now.duration_since(entry.last_refill).as_secs_f64();
            entry.tokens = (entry.tokens + burst * elapsed / window).min(burst);
            entry.last_refill = now;

            if entry.tokens < 1.0 {
                return Err(Rejection::RateLimited);
            }
            entry.tokens -= 1.0;
        }

        entry.active += 1;
        drop(table);

        permit.counted_per_ip = true;
        Ok(permit)
    }

    fn release(&self, ip: IpAddr, counted_per_ip: bool) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        if !counted_per_ip {
            return;
        }
        let mut table = self.per_ip.lock().expect("limiter lock");
        if let Some(entry) = table.get_mut(&ip) {
            entry.active = entry.active.saturating_sub(1);
        }
    }

}

/// Drops entries that hold no connections and whose bucket has refilled.
fn sweep(table: &mut HashMap<IpAddr, IpState>, window: std::time::Duration) {
    let now = Instant::now();
    table.retain(|_, entry| entry.active > 0 || now.duration_since(entry.last_refill) < window);
}

/// Holds a connection slot for the lifetime of the connection.
#[derive(Debug)]
pub struct Permit {
    limiter: Arc<Limiter>,
    ip: IpAddr,
    counted_per_ip: bool,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limiter.release(self.ip, self.counted_per_ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_config::{HumanDuration, IpNet, IpNets, RateLimit};

    fn limits() -> Limits {
        Limits {
            max_connections: 10,
            max_connections_per_ip: 2,
            connection_rate: RateLimit { burst: 3, per: HumanDuration::from_secs(60) },
            max_handshake_bytes: 8192,
            exempt: IpNets::default(),
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn concurrent_connections_per_ip_are_capped_and_released() {
        let limiter = Limiter::new(limits());
        let client = ip("203.0.113.7");

        let first = limiter.admit(client).unwrap();
        let second = limiter.admit(client).unwrap();
        assert_eq!(limiter.admit(client).unwrap_err(), Rejection::PerIpLimit);

        drop(first);
        let third = limiter.admit(client);
        assert!(third.is_ok(), "a closed connection frees the slot");

        drop((second, third));
        assert_eq!(limiter.active(), 0);
    }

    #[test]
    fn a_rejected_connection_does_not_leak_a_global_slot() {
        let limiter = Limiter::new(limits());
        let client = ip("203.0.113.7");
        let _held: Vec<_> = (0..2).map(|_| limiter.admit(client).unwrap()).collect();

        for _ in 0..50 {
            // Each rejection returns a dropped permit, so `active` must not grow.
            assert!(limiter.admit(client).is_err());
        }
        assert_eq!(limiter.active(), 2);
    }

    #[test]
    fn the_rate_bucket_empties_and_refills() {
        let mut limits = limits();
        limits.max_connections_per_ip = 0;
        limits.connection_rate = RateLimit { burst: 3, per: HumanDuration::from_millis(300) };
        let limiter = Limiter::new(limits);
        let client = ip("203.0.113.7");

        // Burst of 3, then the fourth is refused.
        let _burst: Vec<_> = (0..3).map(|_| limiter.admit(client).unwrap()).collect();
        assert_eq!(limiter.admit(client).unwrap_err(), Rejection::RateLimited);

        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(limiter.admit(client).is_ok(), "the bucket refills over time");
    }

    #[test]
    fn different_ips_have_separate_budgets() {
        let limiter = Limiter::new(limits());
        let _a: Vec<_> = (0..2).map(|_| limiter.admit(ip("203.0.113.7")).unwrap()).collect();
        assert!(limiter.admit(ip("203.0.113.8")).is_ok(), "one abuser must not block others");
    }

    #[test]
    fn v4_mapped_addresses_share_one_budget() {
        let limiter = Limiter::new(limits());
        let _a = limiter.admit(ip("203.0.113.7")).unwrap();
        let _b = limiter.admit(ip("::ffff:203.0.113.7")).unwrap();
        assert_eq!(
            limiter.admit(ip("203.0.113.7")).unwrap_err(),
            Rejection::PerIpLimit,
            "the v6-mapped form must not be a second budget"
        );
    }

    #[test]
    fn the_global_limit_applies_across_ips() {
        let mut limits = limits();
        limits.max_connections = 3;
        limits.max_connections_per_ip = 0;
        let limiter = Limiter::new(limits);

        let _held: Vec<_> = (0..3)
            .map(|i| limiter.admit(ip(&format!("203.0.113.{i}"))).unwrap())
            .collect();
        assert_eq!(limiter.admit(ip("203.0.113.9")).unwrap_err(), Rejection::GlobalLimit);
    }

    #[test]
    fn exempt_networks_skip_the_per_ip_limits() {
        let mut limits = limits();
        limits.exempt = IpNets(vec!["10.0.0.0/8".parse::<IpNet>().unwrap()]);
        // The global limit is about gateway resources and still applies; only
        // the per-IP limits are waived.
        limits.max_connections = 0;
        let limiter = Limiter::new(limits);

        let monitor = ip("10.1.2.3");
        let _many: Vec<_> = (0..20).map(|_| limiter.admit(monitor).unwrap()).collect();
        assert_eq!(limiter.tracked_ips(), 0, "exempt IPs are not tracked at all");
    }

    #[test]
    fn idle_entries_are_swept_so_the_table_cannot_grow_without_bound() {
        let mut limits = limits();
        limits.max_connections = 0;
        limits.connection_rate = RateLimit { burst: 1000, per: HumanDuration::from_millis(1) };
        let limiter = Limiter::new(limits);

        for i in 0..SWEEP_THRESHOLD + 100 {
            let octet = i % 256;
            let addr = ip(&format!("198.51.{}.{octet}", (i / 256) % 256));
            drop(limiter.admit(addr));
        }
        assert!(
            limiter.tracked_ips() < SWEEP_THRESHOLD + 100,
            "expected a sweep, still tracking {}",
            limiter.tracked_ips()
        );
    }
}
