//! Minimal CIDR matching, used for the trust boundary of inbound PROXY headers.

use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    str::FromStr,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// An IP network such as `10.0.0.0/8`, `::1/128` or a bare address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    pub fn new(addr: IpAddr, prefix: u8) -> Result<Self, String> {
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(format!("prefix /{prefix} is too long for {addr}"));
        }
        Ok(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 peer arriving on a dual-stack socket shows up as ::ffff:a.b.c.d;
        // comparing it against a v4 rule has to work or every v4 rule silently
        // stops matching the moment the listener binds to [::].
        let ip = unmap(ip);
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                masked_v4(net, self.prefix) == masked_v4(ip, self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                masked_v6(net, self.prefix) == masked_v6(ip, self.prefix)
            }
            _ => false,
        }
    }
}

/// Converts an IPv4-mapped IPv6 address back to plain IPv4.
pub fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

fn masked_v4(ip: Ipv4Addr, prefix: u8) -> u32 {
    let bits = u32::from(ip);
    if prefix == 0 { 0 } else { bits & (!0u32 << (32 - prefix)) }
}

fn masked_v6(ip: Ipv6Addr, prefix: u8) -> u128 {
    let bits = u128::from(ip);
    if prefix == 0 { 0 } else { bits & (!0u128 << (128 - prefix)) }
}

impl FromStr for IpNet {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, prefix)) => {
                let prefix: u8 = prefix
                    .parse()
                    .map_err(|_| format!("invalid prefix length in `{s}`"))?;
                (addr, Some(prefix))
            }
            None => (s, None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| format!("invalid IP address in `{s}`"))?;
        let prefix = prefix.unwrap_or(if addr.is_ipv4() { 32 } else { 128 });
        IpNet::new(addr, prefix)
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl<'de> Deserialize<'de> for IpNet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(de::Error::custom)
    }
}

impl Serialize for IpNet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

/// A list of networks; an empty list matches nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IpNets(pub Vec<IpNet>);

impl IpNets {
    pub fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|net| net.contains(ip))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn matches_v4_prefixes() {
        let net: IpNet = "10.0.0.0/8".parse().unwrap();
        assert!(net.contains(ip("10.1.2.3")));
        assert!(!net.contains(ip("11.1.2.3")));

        let host: IpNet = "203.0.113.7".parse().unwrap();
        assert!(host.contains(ip("203.0.113.7")));
        assert!(!host.contains(ip("203.0.113.8")));
    }

    #[test]
    fn matches_v6_prefixes() {
        let net: IpNet = "2001:db8::/32".parse().unwrap();
        assert!(net.contains(ip("2001:db8:1234::1")));
        assert!(!net.contains(ip("2001:db9::1")));
    }

    #[test]
    fn treats_v4_mapped_peers_as_v4() {
        let net: IpNet = "10.0.0.0/8".parse().unwrap();
        assert!(net.contains(ip("::ffff:10.1.2.3")));
    }

    #[test]
    fn zero_prefix_matches_the_whole_family() {
        let net: IpNet = "0.0.0.0/0".parse().unwrap();
        assert!(net.contains(ip("203.0.113.7")));
        assert!(!net.contains(ip("2001:db8::1")));
    }

    #[test]
    fn rejects_bad_input() {
        assert!("10.0.0.0/33".parse::<IpNet>().is_err());
        assert!("not-an-ip/8".parse::<IpNet>().is_err());
        assert!("10.0.0.0/x".parse::<IpNet>().is_err());
    }
}
