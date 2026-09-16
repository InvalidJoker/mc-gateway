//! Admission control.
//!
//! Everything here runs before a single Minecraft byte is read, because that is
//! the only place where the cost of an abusive connection is still near zero.
//!
//! The token buckets are [`governor`]'s: a keyed rate limiter already handles
//! the per-IP state, the refill arithmetic and the garbage collection that an
//! unbounded map of every IP that ever connected would otherwise need.

use std::{
    net::IpAddr,
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use governor::{Quota, RateLimiter, clock::DefaultClock, state::keyed::DefaultKeyedStateStore};
use mc_config::{Limits, net::unmap};

type IpRateLimiter = RateLimiter<IpAddr, DefaultKeyedStateStore<IpAddr>, DefaultClock>;

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

/// The parts that a config reload replaces wholesale.
struct Policy {
    limits: Limits,
    /// `None` when rate limiting is switched off.
    rate: Option<IpRateLimiter>,
}

impl Policy {
    fn new(limits: Limits) -> Self {
        let rate = quota(&limits).map(RateLimiter::keyed);
        Self { limits, rate }
    }
}

/// Turns `burst` connections per `per` into governor's "one cell every N".
fn quota(limits: &Limits) -> Option<Quota> {
    if limits.connection_rate.is_disabled() {
        return None;
    }
    let burst = NonZeroU32::new(limits.connection_rate.burst)?;
    let replenish = limits.connection_rate.per.checked_div(burst.get())?;
    if replenish.is_zero() {
        return None;
    }
    Some(Quota::with_period(replenish)?.allow_burst(burst))
}

pub struct Limiter {
    policy: ArcSwap<Policy>,
    active: AtomicUsize,
    /// Live connection count per IP. Entries are removed as they reach zero, so
    /// this tracks current clients rather than every visitor ever.
    per_ip: DashMap<IpAddr, usize>,
}

/// Number of tracked buckets above which governor is asked to forget the idle
/// ones.
const GC_THRESHOLD: usize = 4096;

impl Limiter {
    pub fn new(limits: Limits) -> Arc<Self> {
        Arc::new(Self {
            policy: ArcSwap::from_pointee(Policy::new(limits)),
            active: AtomicUsize::new(0),
            per_ip: DashMap::new(),
        })
    }

    /// Applies new limits from a config reload.
    ///
    /// In-flight buckets are reset, since the quota they were built from no
    /// longer exists. Live connection counts are untouched.
    pub fn set_limits(&self, limits: Limits) {
        self.policy.store(Arc::new(Policy::new(limits)));
    }

    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    pub fn tracked_ips(&self) -> usize {
        self.per_ip.len()
    }

    /// Admits a connection, or explains why not.
    ///
    /// The returned permit releases both counters on drop, so no error path can
    /// leak a slot.
    pub fn admit(self: &Arc<Self>, ip: IpAddr) -> Result<Permit, Rejection> {
        // Normalise `::ffff:1.2.3.4` so a dual-stack listener does not give the
        // same client two separate budgets.
        let ip = unmap(ip);
        let policy = self.policy.load();

        // Built before any early return so every rejection path still releases
        // the global slot when it drops.
        let mut permit = Permit { limiter: Arc::clone(self), ip, counted_per_ip: false };

        let global = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        if policy.limits.max_connections != 0 && global > policy.limits.max_connections {
            return Err(Rejection::GlobalLimit);
        }

        if policy.limits.exempt.contains(ip) {
            return Ok(permit);
        }

        // The concurrency cap is checked first, and deliberately does not
        // charge the rate bucket: a client sitting at its connection limit is
        // not the same thing as a client connecting too fast, and mixing the
        // two makes both counters unreadable.
        let max_per_ip = policy.limits.max_connections_per_ip;
        if max_per_ip != 0 && self.per_ip.get(&ip).is_some_and(|count| *count >= max_per_ip) {
            return Err(Rejection::PerIpLimit);
        }

        if let Some(rate) = &policy.rate {
            if rate.len() >= GC_THRESHOLD {
                rate.retain_recent();
            }
            if rate.check_key(&ip).is_err() {
                return Err(Rejection::RateLimited);
            }
        }

        if max_per_ip != 0 {
            // Re-checked under the entry lock: two connections from the same IP
            // must not both pass the read above.
            let mut entry = self.per_ip.entry(ip).or_insert(0);
            if *entry >= max_per_ip {
                return Err(Rejection::PerIpLimit);
            }
            *entry += 1;
            permit.counted_per_ip = true;
        }

        Ok(permit)
    }

    fn release(&self, ip: IpAddr, counted_per_ip: bool) {
        self.active.fetch_sub(1, Ordering::AcqRel);
        if !counted_per_ip {
            return;
        }
        // `remove_if` keeps the map at the size of the live client set.
        if let dashmap::Entry::Occupied(mut entry) = self.per_ip.entry(ip) {
            let value = entry.get_mut();
            *value = value.saturating_sub(1);
            if *value == 0 {
                entry.remove();
            }
        }
    }
}

/// Holds a connection slot for the lifetime of the connection.
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

impl std::fmt::Debug for Permit {
    /// Printed without the limiter it points back at, which would recurse into
    /// the whole per-IP table.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permit")
            .field("ip", &self.ip)
            .field("counted_per_ip", &self.counted_per_ip)
            .finish()
    }
}

/// How long a rate-limited client would have to wait, for logging.
pub fn retry_hint(limits: &Limits) -> Duration {
    limits
        .connection_rate
        .per
        .checked_div(limits.connection_rate.burst.max(1))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mc_config::{IpNet, IpNets, RateLimit};

    fn limits() -> Limits {
        Limits {
            max_connections: 10,
            max_connections_per_ip: 2,
            connection_rate: RateLimit { burst: 3, per: Duration::from_secs(60) },
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
        assert_eq!(limiter.tracked_ips(), 0, "the map shrinks back to live clients");
    }

    #[test]
    fn a_rejected_connection_does_not_leak_a_global_slot() {
        let mut limits = limits();
        // Take the rate limiter out of the picture so the per-IP cap is what
        // rejects.
        limits.connection_rate = RateLimit { burst: 0, per: Duration::from_secs(60) };
        let limiter = Limiter::new(limits);
        let client = ip("203.0.113.7");
        let _held: Vec<_> = (0..2).map(|_| limiter.admit(client).unwrap()).collect();

        for _ in 0..50 {
            assert!(limiter.admit(client).is_err());
        }
        assert_eq!(limiter.active(), 2);
    }

    #[test]
    fn the_rate_bucket_empties_and_refills() {
        let mut limits = limits();
        limits.max_connections_per_ip = 0;
        limits.connection_rate = RateLimit { burst: 3, per: Duration::from_millis(300) };
        let limiter = Limiter::new(limits);
        let client = ip("203.0.113.7");

        let _burst: Vec<_> = (0..3).map(|_| limiter.admit(client).unwrap()).collect();
        assert_eq!(limiter.admit(client).unwrap_err(), Rejection::RateLimited);

        std::thread::sleep(Duration::from_millis(150));
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
    fn a_reload_can_tighten_limits() {
        let limiter = Limiter::new(limits());
        let client = ip("203.0.113.7");
        let _first = limiter.admit(client).unwrap();

        let mut tighter = limits();
        tighter.max_connections_per_ip = 1;
        limiter.set_limits(tighter);

        assert_eq!(limiter.admit(client).unwrap_err(), Rejection::PerIpLimit);
    }

    #[test]
    fn a_disabled_rate_limit_admits_freely() {
        let mut limits = limits();
        limits.max_connections = 0;
        limits.max_connections_per_ip = 0;
        limits.connection_rate = RateLimit { burst: 0, per: Duration::from_secs(60) };
        let limiter = Limiter::new(limits);

        let client = ip("203.0.113.7");
        let held: Vec<_> = (0..100).map(|_| limiter.admit(client).unwrap()).collect();
        assert_eq!(held.len(), 100);
    }
}
