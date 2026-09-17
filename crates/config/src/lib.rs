//! Configuration loading and validation.
//!
//! Validation is split in two: hard errors that stop start-up, and warnings for
//! combinations that parse fine but are almost certainly not what was meant
//! (a group nothing routes to, a server no route can reach).

pub mod intercept;
pub mod model;
pub mod net;

use std::{collections::BTreeSet, path::Path};

pub use intercept::{Intercept, PortRange};
pub use ipnet::IpNet;
pub use model::*;
pub use net::IpNets;

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
        let raw = std::fs::read_to_string(path)
            .map_err(|source| ConfigError::Io { path: display.clone(), source })?;
        Self::parse(&raw, &display)
    }

    pub fn parse(raw: &str, origin: &str) -> Result<Loaded, ConfigError> {
        let config: Config = serde_yaml_ng::from_str(raw)
            .map_err(|source| ConfigError::Parse { path: origin.to_owned(), source })?;
        let warnings = config.validate()?;
        Ok(Loaded { config, warnings })
    }

    /// Names a route may point at: every server and every group.
    pub fn target_names(&self) -> BTreeSet<&str> {
        let mut names: BTreeSet<&str> = self.servers.keys().map(String::as_str).collect();
        names.extend(self.groups.keys().map(String::as_str));
        names.extend(self.servers.values().filter_map(|s| s.group.as_deref()));
        names
    }

    /// Servers belonging to `target`, which is either a server or a group name.
    pub fn resolve_target(&self, target: &str) -> Vec<&str> {
        if let Some((name, _)) = self.servers.get_key_value(target) {
            return vec![name.as_str()];
        }
        self.servers
            .iter()
            .filter(|(_, server)| server.group.as_deref() == Some(target))
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Hard errors abort start-up; the returned warnings are logged.
    pub fn validate(&self) -> Result<Vec<String>, ConfigError> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();

        self.validate_intercept(&mut errors, &mut warnings);
        // A pure interception node has no routed listeners, and then routing
        // and servers have nothing to be validated against.
        if !self.listeners.is_empty() || self.intercept.is_none() {
            self.validate_listeners(&mut errors, &mut warnings);
            self.validate_routing(&mut errors, &mut warnings);
            self.validate_servers(&mut errors, &mut warnings);
        }
        self.validate_operational(&mut warnings);

        if errors.is_empty() { Ok(warnings) } else { Err(ConfigError::Invalid(errors)) }
    }

    fn validate_intercept(&self, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
        let Some(intercept) = &self.intercept else { return };

        if intercept.ports.is_empty() {
            errors.push("intercept.ports is empty".into());
        }
        if intercept.covers(intercept.listen.port()) {
            errors.push(format!(
                "intercept.listen port {} lies inside intercept.ports; connections would be \
                 handed back to the gateway in a loop",
                intercept.listen.port()
            ));
        }
        if !intercept.listen.is_ipv4() {
            errors.push("intercept.listen must be an IPv4 address".into());
        }
        if let Some(listen_v6) = intercept.listen_v6 {
            if !listen_v6.is_ipv6() {
                errors.push("intercept.listen_v6 must be an IPv6 address".into());
            }
            if intercept.covers(listen_v6.port()) {
                errors.push(format!(
                    "intercept.listen_v6 port {} lies inside intercept.ports; connections would \
                     be handed back to the gateway in a loop",
                    listen_v6.port()
                ));
            }
        }
        for listener in &self.listeners {
            if intercept.covers(listener.bind.port()) {
                errors.push(format!(
                    "listener `{}` binds port {}, which intercept.ports takes over",
                    listener.name,
                    listener.bind.port()
                ));
            }
        }
        for (index, range) in intercept.ports.iter().enumerate() {
            for other in &intercept.ports[index + 1..] {
                if range.overlaps(other) {
                    warnings.push(format!("intercept.ports `{range}` and `{other}` overlap"));
                }
            }
        }
        if !self.motd.rewrites_anything() {
            warnings.push(
                "intercept is configured but motd.line1/line2 are not: every connection is \
                 passed through untouched"
                    .into(),
            );
        }
        if !cfg!(target_os = "linux") {
            warnings.push(
                "intercept needs Linux TPROXY; this build can validate the config but not run it"
                    .into(),
            );
        }
    }

    fn validate_listeners(&self, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
        if self.listeners.is_empty() {
            errors.push("no listeners configured".into());
        }

        let mut seen_names = BTreeSet::new();
        let mut seen_binds = BTreeSet::new();
        for listener in &self.listeners {
            if listener.name.is_empty() {
                errors.push("a listener has an empty name".into());
            }
            if !seen_names.insert(listener.name.as_str()) {
                errors.push(format!("duplicate listener name `{}`", listener.name));
            }
            if !seen_binds.insert(listener.bind) {
                errors.push(format!("two listeners bind to {}", listener.bind));
            }
            // Honouring a PROXY header from anyone lets any client claim any
            // source IP, defeating every per-IP limit and poisoning the logs.
            if listener.proxy_protocol.enabled && listener.proxy_protocol.trusted.is_empty() {
                errors.push(format!(
                    "listener `{}` enables inbound proxy_protocol without `trusted` networks; \
                     an untrusted peer could spoof any client IP",
                    listener.name
                ));
            }
            if listener.proxy_protocol.required && !listener.proxy_protocol.enabled {
                warnings.push(format!(
                    "listener `{}` sets proxy_protocol.required but not proxy_protocol.enabled",
                    listener.name
                ));
            }
        }
    }

    fn validate_routing(&self, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
        let targets = self.target_names();
        let listener_names: BTreeSet<&str> =
            self.listeners.iter().map(|l| l.name.as_str()).collect();

        if self.routing.rules.is_empty() && self.routing.default.is_none() {
            errors.push("routing has neither rules nor a default target".into());
        }

        if let Some(default) = &self.routing.default {
            if !targets.contains(default.as_str()) {
                errors.push(format!("routing.default `{default}` is not a server or group"));
            } else if self.resolve_target(default).is_empty() {
                errors.push(format!("routing.default `{default}` is a group with no members"));
            }
        } else {
            warnings.push(
                "no routing.default: clients that send no hostname (pre-1.7 pings) are refused"
                    .into(),
            );
        }

        let mut seen_hosts = BTreeSet::new();
        for rule in &self.routing.rules {
            if let Err(why) = validate_host_pattern(&rule.host) {
                errors.push(format!("route `{}`: {why}", rule.host));
            }
            if !targets.contains(rule.target.as_str()) {
                errors.push(format!(
                    "route `{}` points at `{}`, which is not a server or group",
                    rule.host, rule.target
                ));
            }
            if self.resolve_target(&rule.target).is_empty() {
                errors.push(format!(
                    "route `{}` points at group `{}`, which has no members",
                    rule.host, rule.target
                ));
            }
            let key = (rule.host.to_ascii_lowercase(), rule.listeners.clone());
            if !seen_hosts.insert(key) {
                warnings.push(format!(
                    "route `{}` is declared more than once; the first one wins",
                    rule.host
                ));
            }
            for name in rule.listeners.iter().flatten() {
                if !listener_names.contains(name.as_str()) {
                    errors.push(format!(
                        "route `{}` restricts to listener `{name}`, which does not exist",
                        rule.host
                    ));
                }
            }
        }
    }

    fn validate_servers(&self, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
        let mut routed: BTreeSet<&str> = BTreeSet::new();
        for target in self
            .routing
            .rules
            .iter()
            .map(|r| r.target.as_str())
            .chain(self.routing.default.as_deref())
        {
            routed.extend(self.resolve_target(target));
        }

        for (name, server) in &self.servers {
            if let Err(why) = validate_address(&server.address) {
                errors.push(format!("server `{name}`: {why}"));
            }
            if server.weight == 0 {
                warnings.push(format!("server `{name}` has weight 0 and will never be selected"));
            }
            if !routed.contains(name.as_str()) {
                warnings.push(format!("server `{name}` is not reachable through any route"));
            }
        }

        for name in self.groups.keys() {
            if self.resolve_target(name).is_empty() {
                warnings.push(format!("group `{name}` has no members"));
            }
        }
    }

    fn validate_operational(&self, warnings: &mut Vec<String>) {
        if self.health.enabled && self.health.timeout >= self.health.interval {
            warnings.push(format!(
                "health.timeout ({:?}) is not shorter than health.interval ({:?}); \
                 checks will overlap",
                self.health.timeout, self.health.interval
            ));
        }
        if self.limits.max_connections_per_ip == 0 {
            warnings.push("limits.max_connections_per_ip is 0 (unlimited)".into());
        }
        if self.limits.connection_rate.is_disabled() {
            warnings.push("limits.connection_rate is disabled".into());
        }
        if self.metrics.enabled && self.metrics.bind.ip().is_unspecified() {
            warnings.push(format!(
                "metrics listen on {}, which is world-reachable; bind them to a private address",
                self.metrics.bind
            ));
        }
        if self.timeouts.idle.is_zero() {
            warnings.push(
                "timeouts.idle is 0: a client that vanishes without closing holds its backend \
                 slot forever"
                    .into(),
            );
        }
    }
}

/// `example.net`, `*.example.net` or `*`.
pub fn validate_host_pattern(pattern: &str) -> Result<(), String> {
    if pattern.is_empty() {
        return Err("empty host pattern".into());
    }
    if pattern == "*" {
        return Ok(());
    }
    let body = pattern.strip_prefix("*.").unwrap_or(pattern);
    if body.is_empty() {
        return Err("wildcard needs a domain after `*.`".into());
    }
    if body.contains('*') {
        return Err("`*` is only allowed as a leading `*.` label".into());
    }
    if body.contains('/') || body.contains(' ') {
        return Err("host pattern contains an invalid character".into());
    }
    if body.contains(':') {
        return Err("host pattern must not contain a port".into());
    }
    Ok(())
}

/// Backend addresses are `host:port`; the host may be a DNS name.
pub fn validate_address(address: &str) -> Result<(), String> {
    let (host, port) = split_host_port(address)?;
    if host.is_empty() {
        return Err(format!("`{address}` has an empty host"));
    }
    if port == 0 {
        return Err(format!("`{address}` has port 0"));
    }
    Ok(())
}

/// Splits `host:port`, including the `[::1]:25565` form.
pub fn split_host_port(address: &str) -> Result<(&str, u16), String> {
    let (host, port) = if let Some(rest) = address.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .ok_or_else(|| format!("`{address}` is missing the closing `]`"))?;
        let port = rest
            .strip_prefix(':')
            .ok_or_else(|| format!("`{address}` is missing a port"))?;
        (host, port)
    } else {
        address
            .rsplit_once(':')
            .ok_or_else(|| format!("`{address}` is missing a port (expected host:port)"))?
    };

    let port: u16 = port.parse().map_err(|_| format!("`{address}` has an invalid port"))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MINIMAL: &str = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
routing:
  default: survival
servers:
  survival-01:
    address: "10.10.1.10:25565"
    group: survival
"#;

    fn load(raw: &str) -> Result<Loaded, ConfigError> {
        Config::parse(raw, "test")
    }

    #[test]
    fn parses_a_minimal_config() {
        let loaded = load(MINIMAL).unwrap();
        assert_eq!(loaded.config.listeners.len(), 1);
        assert_eq!(loaded.config.resolve_target("survival"), ["survival-01"]);
        assert_eq!(loaded.config.resolve_target("survival-01"), ["survival-01"]);
    }

    #[test]
    fn without_motd_lines_nothing_is_rewritten() {
        // The default is full pass-through: the backend's own status response
        // reaches the client untouched.
        assert!(!load(MINIMAL).unwrap().config.motd.rewrites_anything());
    }

    #[test]
    fn a_single_motd_line_turns_rewriting_on() {
        let raw = format!("{MINIMAL}motd:\n  line2: \"&7my network\"\n");
        let motd = load(&raw).unwrap().config.motd;
        assert!(motd.rewrites_anything());
        assert_eq!(motd.line2.as_deref(), Some("&7my network"));
        assert_eq!(motd.line1, None, "the backend keeps its first line");
    }

    #[test]
    fn durations_are_read_as_human_text() {
        let raw = format!(
            "{MINIMAL}timeouts:\n  handshake: 2s\n  idle: 5m\n  connect: 750ms\n"
        );
        let timeouts = load(&raw).unwrap().config.timeouts;
        assert_eq!(timeouts.handshake, Duration::from_secs(2));
        assert_eq!(timeouts.idle, Duration::from_secs(300));
        assert_eq!(timeouts.connect, Duration::from_millis(750));
        assert_eq!(timeouts.status, Duration::from_secs(10), "unset keeps the default");
    }

    #[test]
    fn rejects_unknown_keys() {
        let err = load(&format!("{MINIMAL}\nmodt:\n  line2: typo\n")).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn rejects_routes_to_nowhere() {
        let raw = MINIMAL.replace("default: survival", "default: creative");
        let err = load(&raw).unwrap_err();
        assert!(err.to_string().contains("not a server or group"), "{err}");
    }

    #[test]
    fn rejects_untrusted_inbound_proxy_protocol() {
        let raw = MINIMAL.replace(
            "    bind: \"0.0.0.0:25565\"",
            "    bind: \"0.0.0.0:25565\"\n    proxy_protocol:\n      enabled: true",
        );
        let err = load(&raw).unwrap_err();
        assert!(err.to_string().contains("spoof any client IP"), "{err}");
    }

    #[test]
    fn accepts_inbound_proxy_protocol_with_a_trust_list() {
        let raw = MINIMAL.replace(
            "    bind: \"0.0.0.0:25565\"",
            "    bind: \"0.0.0.0:25565\"\n    proxy_protocol:\n      enabled: true\n      trusted: [\"10.0.0.0/8\"]",
        );
        assert!(load(&raw).is_ok());
    }

    #[test]
    fn warns_about_unrouted_servers() {
        let raw = format!("{MINIMAL}  creative-01:\n    address: \"10.10.2.10:25565\"\n");
        let loaded = load(&raw).unwrap();
        assert!(
            loaded.warnings.iter().any(|w| w.contains("creative-01") && w.contains("not reachable")),
            "{:?}",
            loaded.warnings
        );
    }

    #[test]
    fn rejects_a_group_with_no_members() {
        let raw = format!(
            "{}groups:\n  creative: {{}}\n",
            MINIMAL.replace("default: survival", "default: creative")
        );
        let err = load(&raw).unwrap_err();
        assert!(err.to_string().contains("no members"), "{err}");
    }

    #[test]
    fn validates_host_patterns() {
        assert!(validate_host_pattern("example.net").is_ok());
        assert!(validate_host_pattern("*.example.net").is_ok());
        assert!(validate_host_pattern("*").is_ok());
        assert!(validate_host_pattern("ex*.net").is_err());
        assert!(validate_host_pattern("example.net:25565").is_err());
        assert!(validate_host_pattern("").is_err());
    }

    #[test]
    fn splits_addresses_including_ipv6() {
        assert_eq!(split_host_port("10.0.0.1:25565").unwrap(), ("10.0.0.1", 25565));
        assert_eq!(split_host_port("[::1]:25565").unwrap(), ("::1", 25565));
        assert_eq!(split_host_port("mc.internal:25577").unwrap(), ("mc.internal", 25577));
        assert!(split_host_port("10.0.0.1").is_err());
        assert!(split_host_port("10.0.0.1:99999").is_err());
    }

    #[test]
    fn route_motd_overlays_the_global_one() {
        let raw = r#"
listeners:
  - name: public
    bind: "0.0.0.0:25565"
motd:
  line1: "&bglobal"
  line2: "&7global second"
routing:
  rules:
    - host: "modded.example.net"
      target: modded-01
      motd:
        line2: "&6modded only"
servers:
  modded-01:
    address: "10.10.2.10:25565"
"#;
        let config = load(raw).unwrap().config;
        let effective = config.motd.overlay(config.routing.rules[0].motd.as_ref());
        assert_eq!(effective.line2.as_deref(), Some("&6modded only"));
        assert_eq!(effective.line1.as_deref(), Some("&bglobal"), "unset fields fall back");
    }

    const INTERCEPT_ONLY: &str = r#"
intercept:
  ports: ["30000-40000"]
motd:
  line2: "&7Hosted by example.net"
"#;

    #[test]
    fn an_intercept_only_config_needs_no_routing() {
        let loaded = load(INTERCEPT_ONLY).unwrap();
        let intercept = loaded.config.intercept.unwrap();
        assert!(intercept.covers(30123));
        assert!(loaded.config.listeners.is_empty());
    }

    #[test]
    fn rejects_an_intercept_listener_inside_its_own_range() {
        let raw = INTERCEPT_ONLY.replace("intercept:\n", "intercept:\n  listen: \"127.0.0.1:30500\"\n");
        let err = load(&raw).unwrap_err();
        assert!(err.to_string().contains("in a loop"), "{err}");
    }

    #[test]
    fn rejects_listeners_of_the_wrong_family() {
        let raw = INTERCEPT_ONLY.replace("intercept:\n", "intercept:\n  listen_v6: \"127.0.0.1:25501\"\n");
        let err = load(&raw).unwrap_err();
        assert!(err.to_string().contains("listen_v6 must be an IPv6 address"), "{err}");
    }

    #[test]
    fn warns_when_nothing_would_be_rewritten() {
        let loaded = load("intercept:\n  ports: [30000]\n").unwrap();
        assert!(loaded.warnings.iter().any(|w| w.contains("passed through untouched")));
    }

    #[test]
    fn an_empty_config_is_still_rejected() {
        assert!(load("motd:\n  line2: x\n").is_err());
    }

    #[test]
    fn the_shipped_node_config_is_valid() {
        let raw = include_str!("../../../deploy/node/config.yaml");
        let loaded = Config::parse(raw, "deploy/node/config.yaml").expect("node config loads");
        let intercept = loaded.config.intercept.expect("an interception config");
        assert!(intercept.covers(25565));
        assert!(loaded.config.motd.line2.is_some());
        let unexpected: Vec<_> =
            loaded.warnings.iter().filter(|w| !w.contains("needs Linux")).collect();
        assert!(unexpected.is_empty(), "{unexpected:?}");
    }

    #[test]
    fn the_shipped_example_config_is_valid() {
        let raw = include_str!("../../../config.example.yaml");
        let loaded = Config::parse(raw, "config.example.yaml")
            .expect("the example config must always load");
        assert!(loaded.config.servers.len() >= 3);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
    }
}
