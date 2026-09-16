//! Thin wrapper around [`ipnet`] for the one thing the gateway needs from it.

use std::net::IpAddr;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

/// A list of networks. An empty list matches nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IpNets(pub Vec<IpNet>);

impl IpNets {
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = unmap(ip);
        self.0.iter().any(|net| net.contains(&ip))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Converts an IPv4-mapped IPv6 address back to plain IPv4.
///
/// A dual-stack listener reports IPv4 peers as `::ffff:a.b.c.d`. Without this
/// every IPv4 rule would silently stop matching the moment the listener binds
/// to `[::]`, and every IPv4 client would get its own fresh limit budget.
pub fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nets(values: &[&str]) -> IpNets {
        IpNets(values.iter().map(|v| v.parse().unwrap()).collect())
    }

    fn ip(value: &str) -> IpAddr {
        value.parse().unwrap()
    }

    #[test]
    fn matches_prefixes_of_both_families() {
        let list = nets(&["10.0.0.0/8", "2001:db8::/32"]);
        assert!(list.contains(ip("10.1.2.3")));
        assert!(list.contains(ip("2001:db8:1234::1")));
        assert!(!list.contains(ip("11.1.2.3")));
    }

    #[test]
    fn treats_v4_mapped_peers_as_v4() {
        assert!(nets(&["10.0.0.0/8"]).contains(ip("::ffff:10.1.2.3")));
    }

    #[test]
    fn an_empty_list_matches_nothing() {
        assert!(!IpNets::default().contains(ip("10.1.2.3")));
        assert!(IpNets::default().is_empty());
    }

    #[test]
    fn a_bare_address_is_a_host_route() {
        let list = nets(&["203.0.113.7/32"]);
        assert!(list.contains(ip("203.0.113.7")));
        assert!(!list.contains(ip("203.0.113.8")));
    }
}
