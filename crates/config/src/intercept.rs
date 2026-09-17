//! Port-range interception: the hosting-node mode.
//!
//! Instead of routing by hostname to a configured server list, the gateway
//! sits in front of every game server on a node and takes over connections to
//! a range of ports. Each connection goes to exactly the server the player
//! dialled; the only visible change is the MOTD line the node owner sets.

use std::{fmt, net::SocketAddr, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Intercept {
    /// Where intercepted connections are delivered. Loopback is enough: the
    /// kernel hands connections over regardless of the address players dialled,
    /// and it keeps the port unreachable from outside.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// Ports whose connections are taken over, e.g. `["25565-25665", 30000]`.
    pub ports: Vec<PortRange>,
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:25500".parse().expect("valid default")
}

impl Intercept {
    pub fn covers(&self, port: u16) -> bool {
        self.ports.iter().any(|range| range.contains(port))
    }
}

/// An inclusive range of ports, written `30000-40000` or as a single port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    pub fn contains(&self, port: u16) -> bool {
        (self.start..=self.end).contains(&port)
    }

    pub fn overlaps(&self, other: &PortRange) -> bool {
        self.start <= other.end && other.start <= self.end
    }
}

impl fmt::Display for PortRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

impl FromStr for PortRange {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parse = |part: &str| -> Result<u16, String> {
            let port: u16 = part.trim().parse().map_err(|_| format!("`{part}` is not a port"))?;
            if port == 0 {
                return Err("port 0 cannot be intercepted".into());
            }
            Ok(port)
        };
        let (start, end) = match s.split_once('-') {
            Some((start, end)) => (parse(start)?, parse(end)?),
            None => {
                let port = parse(s)?;
                (port, port)
            }
        };
        if start > end {
            return Err(format!("range `{s}` ends before it starts"));
        }
        Ok(PortRange { start, end })
    }
}

impl<'de> Deserialize<'de> for PortRange {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = PortRange;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a port or a range such as `30000-40000`")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                let port = u16::try_from(value).map_err(|_| E::custom("port out of range"))?;
                port.to_string().parse().map_err(E::custom)
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                let port = u16::try_from(value).map_err(|_| E::custom("port out of range"))?;
                port.to_string().parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl Serialize for PortRange {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges_and_single_ports() {
        assert_eq!("30000-40000".parse::<PortRange>().unwrap(), PortRange { start: 30000, end: 40000 });
        assert_eq!("25565".parse::<PortRange>().unwrap(), PortRange { start: 25565, end: 25565 });
        assert_eq!(" 1 - 2 ".parse::<PortRange>().unwrap(), PortRange { start: 1, end: 2 });
    }

    #[test]
    fn rejects_nonsense() {
        assert!("40000-30000".parse::<PortRange>().is_err());
        assert!("0-10".parse::<PortRange>().is_err());
        assert!("70000".parse::<PortRange>().is_err());
        assert!("a-b".parse::<PortRange>().is_err());
    }

    #[test]
    fn reads_both_yaml_forms() {
        let intercept: Intercept =
            serde_yaml_ng::from_str("ports: [\"25565-25665\", 30000]").unwrap();
        assert_eq!(intercept.ports.len(), 2);
        assert!(intercept.covers(25600));
        assert!(intercept.covers(30000));
        assert!(!intercept.covers(30001));
        assert_eq!(intercept.listen, default_listen());
    }

    #[test]
    fn detects_overlap() {
        let a: PortRange = "100-200".parse().unwrap();
        assert!(a.overlaps(&"200-300".parse().unwrap()));
        assert!(!a.overlaps(&"201-300".parse().unwrap()));
    }
}
