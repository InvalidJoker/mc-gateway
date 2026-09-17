//! Configuration: which ports to intercept, and what to put in the MOTD.
//!
//! ```yaml
//! ports: ["25565-25665"]
//! motd:
//!   line2: "&7Hosted by &bexample.net"
//! ```
//!
//! Everything else has a default. Unknown keys are rejected, so a typo fails at
//! start-up instead of silently changing behaviour.

use std::{fmt, net::SocketAddr, path::Path, str::FromStr, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Ports whose connections are taken over, e.g. `["25565-25665", 30000]`.
    pub ports: Vec<PortRange>,
    /// Where the kernel hands intercepted IPv4 connections over. Loopback keeps
    /// it unreachable from outside.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// The same for IPv6. `null` leaves IPv6 connections alone.
    #[serde(default = "default_listen_v6")]
    pub listen_v6: Option<SocketAddr>,
    #[serde(default)]
    pub motd: Motd,
    #[serde(default)]
    pub offline: Offline,
    #[serde(default)]
    pub timeouts: Timeouts,
    #[serde(default)]
    pub metrics: Metrics,
    #[serde(default)]
    pub log: Log,
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:25500".parse().expect("valid default")
}

fn default_listen_v6() -> Option<SocketAddr> {
    Some("[::1]:25500".parse().expect("valid default"))
}

/// The lines replaced in every intercepted server's MOTD. Unset lines stay the
/// server's own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Motd {
    pub line1: Option<String>,
    pub line2: Option<String>,
}

impl Motd {
    /// Whether status pings have to be looked at at all.
    pub fn rewrites_anything(&self) -> bool {
        self.line1.is_some() || self.line2.is_some()
    }
}

/// What players see when the server behind a port does not answer — stopped,
/// crashed, or no server allocated to that port at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Offline {
    /// When off, an unreachable server's port simply closes the connection.
    pub enabled: bool,
    /// The server list MOTD. `motd.line1`/`line2` still apply on top of it.
    pub motd: String,
    /// Shown in red where the ping bars would be.
    pub version: String,
    /// Disconnect message for a player trying to join. `null` closes the
    /// connection without one.
    pub kick: Option<String>,
}

impl Default for Offline {
    fn default() -> Self {
        Self {
            enabled: true,
            motd: "&cThis server is offline".into(),
            version: "&cOffline".into(),
            kick: Some("&cThis server is offline right now.\n&7Try again later.".into()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    /// How long to wait for a client to show whether it is a status ping.
    /// Anything still undecided is passed through.
    #[serde(with = "humantime_serde")]
    pub handshake: Duration,
    /// How long to wait for the server's status response.
    #[serde(with = "humantime_serde")]
    pub status: Duration,
    /// Connecting to the server.
    #[serde(with = "humantime_serde")]
    pub connect: Duration,
    /// How long open connections get to finish on shutdown.
    #[serde(with = "humantime_serde")]
    pub drain: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(5),
            status: Duration::from_secs(10),
            connect: Duration::from_secs(3),
            drain: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metrics {
    pub enabled: bool,
    /// Prometheus scrape endpoint. Keep it off the public interface.
    pub bind: SocketAddr,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: "127.0.0.1:9100".parse().expect("valid default"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Log {
    pub level: String,
    pub format: LogFormat,
    /// Include player addresses in logs.
    pub client_ip: bool,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: LogFormat::Text,
            client_ip: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}

// ------------------------------------------------------------------ ports --

/// An inclusive range of ports, written `30000-40000` or as a single port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            let port: u16 = part
                .trim()
                .parse()
                .map_err(|_| format!("`{part}` is not a port"))?;
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
                value.to_string().parse().map_err(E::custom)
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                value.to_string().parse().map_err(E::custom)
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

// ------------------------------------------------------ loading/validation --

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },
    #[error("invalid configuration:\n{}", .0.iter().map(|e| format!("  - {e}")).collect::<Vec<_>>().join("\n"))]
    Invalid(Vec<String>),
}

/// A parsed configuration together with any non-fatal complaints.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Loaded, ConfigError> {
        let path = path.as_ref();
        let display = path.display().to_string();
        let raw = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: display.clone(),
            source,
        })?;
        Self::parse(&raw, &display)
    }

    pub fn parse(raw: &str, origin: &str) -> Result<Loaded, ConfigError> {
        let config: Config = serde_yaml_ng::from_str(raw).map_err(|source| ConfigError::Parse {
            path: origin.to_owned(),
            source,
        })?;
        let warnings = config.validate()?;
        Ok(Loaded { config, warnings })
    }

    pub fn covers(&self, port: u16) -> bool {
        self.ports.iter().any(|range| range.contains(port))
    }

    /// Hard errors abort start-up; the returned warnings are logged.
    pub fn validate(&self) -> Result<Vec<String>, ConfigError> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        if self.ports.is_empty() {
            errors.push("`ports` is empty".into());
        }
        if !self.listen.is_ipv4() {
            errors.push("`listen` must be an IPv4 address".into());
        }
        if self.listen_v6.is_some_and(|listen| !listen.is_ipv6()) {
            errors.push("`listen_v6` must be an IPv6 address".into());
        }
        for (name, listen) in [("listen", Some(self.listen)), ("listen_v6", self.listen_v6)] {
            if let Some(listen) = listen
                && self.covers(listen.port())
            {
                errors.push(format!(
                    "`{name}` port {} lies inside `ports`; connections would loop back into the \
                     gateway",
                    listen.port()
                ));
            }
        }
        for (index, range) in self.ports.iter().enumerate() {
            for other in &self.ports[index + 1..] {
                if range.overlaps(other) {
                    warnings.push(format!("ports `{range}` and `{other}` overlap"));
                }
            }
        }
        if !self.motd.rewrites_anything() {
            warnings.push(
                "motd.line1 and motd.line2 are unset: every connection passes through untouched"
                    .into(),
            );
        }
        if self.metrics.enabled && self.metrics.bind.ip().is_unspecified() {
            warnings.push(format!(
                "metrics listen on {}, which is reachable from outside",
                self.metrics.bind
            ));
        }
        if !cfg!(target_os = "linux") {
            warnings.push("interception needs Linux; this build can only check the config".into());
        }

        if errors.is_empty() {
            Ok(warnings)
        } else {
            Err(ConfigError::Invalid(errors))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "ports: [\"30000-40000\"]\nmotd:\n  line2: \"&7Hosted by example.net\"\n";

    fn load(raw: &str) -> Result<Loaded, ConfigError> {
        Config::parse(raw, "test")
    }

    #[test]
    fn a_minimal_config_gets_sensible_defaults() {
        let config = load(MINIMAL).unwrap().config;
        assert!(config.covers(30123));
        assert!(!config.covers(29999));
        assert_eq!(config.listen, default_listen());
        assert_eq!(
            config.listen_v6,
            default_listen_v6(),
            "IPv6 is on by default"
        );
        assert_eq!(config.timeouts.handshake, Duration::from_secs(5));
        assert!(
            !config.log.client_ip,
            "player addresses are not logged by default"
        );
    }

    #[test]
    fn port_ranges_parse_both_forms() {
        let config = load("ports: [\"25565-25665\", 30000]\nmotd: {line2: x}\n")
            .unwrap()
            .config;
        assert_eq!(
            config.ports,
            [
                PortRange {
                    start: 25565,
                    end: 25665
                },
                PortRange {
                    start: 30000,
                    end: 30000
                }
            ]
        );
        for bad in ["40000-30000", "0-10", "70000", "a-b"] {
            assert!(bad.parse::<PortRange>().is_err(), "{bad}");
        }
    }

    #[test]
    fn offline_answers_are_on_by_default_and_configurable() {
        let default = load(MINIMAL).unwrap().config.offline;
        assert!(default.enabled);
        assert!(default.kick.is_some());

        let custom = load(&format!(
            "{MINIMAL}offline:\n  motd: \"&cSleeping\"\n  version: \"&cZzz\"\n  kick: null\n"
        ))
        .unwrap()
        .config
        .offline;
        assert_eq!(custom.motd, "&cSleeping");
        assert_eq!(custom.version, "&cZzz");
        assert_eq!(custom.kick, None);
        assert!(custom.enabled, "unset keys keep their defaults");
    }

    #[test]
    fn ipv6_can_be_turned_off() {
        let config = load(&format!("{MINIMAL}listen_v6: null\n")).unwrap().config;
        assert_eq!(config.listen_v6, None);
    }

    #[test]
    fn durations_are_human_text() {
        let config = load(&format!(
            "{MINIMAL}timeouts:\n  handshake: 2s\n  connect: 750ms\n"
        ))
        .unwrap()
        .config;
        assert_eq!(config.timeouts.handshake, Duration::from_secs(2));
        assert_eq!(config.timeouts.connect, Duration::from_millis(750));
    }

    #[test]
    fn typos_are_rejected() {
        assert!(matches!(
            load(&format!("{MINIMAL}modt: {{}}\n")),
            Err(ConfigError::Parse { .. })
        ));
        assert!(
            matches!(load("motd: {line2: x}\n"), Err(ConfigError::Parse { .. })),
            "ports is required"
        );
    }

    #[test]
    fn a_listener_inside_the_range_would_loop() {
        let err = load(&format!("{MINIMAL}listen: \"127.0.0.1:30500\"\n")).unwrap_err();
        assert!(err.to_string().contains("loop"), "{err}");
    }

    #[test]
    fn listeners_must_match_their_family() {
        let err = load(&format!("{MINIMAL}listen_v6: \"127.0.0.1:25501\"\n")).unwrap_err();
        assert!(err.to_string().contains("must be an IPv6 address"), "{err}");
    }

    #[test]
    fn nothing_to_rewrite_is_worth_a_warning() {
        let loaded = load("ports: [30000]\n").unwrap();
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.contains("passes through untouched"))
        );
    }

    #[test]
    fn the_shipped_config_is_valid() {
        let loaded =
            Config::parse(include_str!("../deploy/config.yaml"), "deploy/config.yaml").unwrap();
        assert!(loaded.config.motd.line2.is_some());
        let unexpected: Vec<_> = loaded
            .warnings
            .iter()
            .filter(|w| !w.contains("needs Linux"))
            .collect();
        assert!(unexpected.is_empty(), "{unexpected:?}");
    }
}
