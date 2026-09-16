//! The configuration model.
//!
//! Every section has a `Default` that is safe to run with, so a minimal file
//! only needs a listener, a route and a server. Unknown keys are rejected so a
//! typo fails at start-up instead of silently changing behaviour in production.

use std::{collections::BTreeMap, fmt, net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{cidr::IpNets, duration::HumanDuration};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listeners: Vec<Listener>,
    pub routing: Routing,
    /// Backend servers by name. 300 entries here is the design point.
    pub servers: BTreeMap<String, Server>,
    /// Optional per-group settings; a group referenced by a server but absent
    /// here uses the defaults.
    pub groups: BTreeMap<String, Group>,
    pub motd: Motd,
    pub messages: Messages,
    pub limits: Limits,
    pub timeouts: Timeouts,
    pub health: Health,
    pub metrics: Metrics,
    pub log: Log,
}

// ---------------------------------------------------------------- listeners

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listener {
    pub name: String,
    pub bind: SocketAddr,
    /// Accept a PROXY protocol header from an upstream L4 load balancer.
    #[serde(default)]
    pub proxy_protocol: InboundProxyProtocol,
    /// TCP backlog handed to `listen(2)`.
    #[serde(default = "default_backlog")]
    pub backlog: u32,
}

fn default_backlog() -> u32 {
    1024
}

/// Trusting an inbound PROXY header means trusting whoever sends it to state
/// the client's IP. It is therefore off by default and gated on a source-IP
/// allowlist that must not be empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InboundProxyProtocol {
    pub enabled: bool,
    /// Only headers from these networks are honoured.
    pub trusted: IpNets,
    /// Reject connections from trusted networks that arrive without a header.
    pub required: bool,
}

// ------------------------------------------------------------------ routing

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    /// Target used when no rule matches. Without it, unmatched hosts are
    /// refused — which is the safer default for a public edge.
    pub default: Option<String>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Exact host (`survival.example.net`), wildcard (`*.example.net`) or `*`.
    pub host: String,
    /// A server name or a group name.
    pub target: String,
    /// Restrict this rule to specific listeners.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listeners: Option<Vec<String>>,
    /// Per-route MOTD overlay on top of the global one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motd: Option<MotdOverride>,
}

// ------------------------------------------------------------------ servers

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// `host:port`. A DNS name is resolved on every connect attempt, so
    /// container restarts with a new address are picked up without a reload.
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Metadata used for logs, metrics labels and config sanity checks. It
    /// never selects a forwarding mode on its own.
    #[serde(default)]
    pub kind: BackendKind,
    #[serde(default)]
    pub forwarding: Forwarding,
    /// Relative share for round-robin selection.
    #[serde(default = "default_weight")]
    pub weight: u32,
    /// Cap on simultaneous sessions to this server; 0 means unlimited.
    #[serde(default)]
    pub max_connections: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthOverride>,
}

fn default_weight() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Vanilla,
    Paper,
    Spigot,
    Folia,
    Velocity,
    Bungeecord,
    Fabric,
    Forge,
    Neoforge,
    Quilt,
    #[default]
    Unknown,
}

impl BackendKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            BackendKind::Vanilla => "vanilla",
            BackendKind::Paper => "paper",
            BackendKind::Spigot => "spigot",
            BackendKind::Folia => "folia",
            BackendKind::Velocity => "velocity",
            BackendKind::Bungeecord => "bungeecord",
            BackendKind::Fabric => "fabric",
            BackendKind::Forge => "forge",
            BackendKind::Neoforge => "neoforge",
            BackendKind::Quilt => "quilt",
            BackendKind::Unknown => "unknown",
        }
    }

    /// Whether this software can read a PROXY protocol header at all.
    ///
    /// Vanilla and the modloaders built on it cannot; only the Bukkit-family
    /// servers and the Java proxies grew an option for it.
    pub const fn supports_proxy_protocol(self) -> bool {
        matches!(
            self,
            BackendKind::Paper
                | BackendKind::Spigot
                | BackendKind::Folia
                | BackendKind::Velocity
                | BackendKind::Bungeecord
                | BackendKind::Unknown
        )
    }
}

impl fmt::Display for BackendKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the backend learns the real client IP.
///
/// Deliberately limited to methods that do not require terminating login at the
/// gateway. Velocity's "modern" forwarding and BungeeCord's `\0`-suffixed
/// handshake both carry an *authenticated identity*, which only a proxy that
/// has itself authenticated the player may assert. This gateway never does,
/// so it never claims to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Forwarding {
    /// Plain TCP. The backend sees the gateway's address.
    #[default]
    None,
    /// HAProxy PROXY protocol v2 header before the first Minecraft byte.
    /// Paper: `proxy-protocol: true`. Velocity: `haproxy-protocol = true`.
    ProxyProtocolV2,
    /// Linux TPROXY: the outbound socket is bound to the client's own address,
    /// so the backend sees the real IP with no protocol support required.
    Transparent,
}

impl Forwarding {
    pub const fn as_str(self) -> &'static str {
        match self {
            Forwarding::None => "none",
            Forwarding::ProxyProtocolV2 => "proxy_protocol_v2",
            Forwarding::Transparent => "transparent",
        }
    }
}

impl fmt::Display for Forwarding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Group {
    pub policy: Policy,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Weighted round robin across healthy members.
    #[default]
    RoundRobin,
    /// Fewest active sessions first.
    LeastConnections,
    /// Always the first healthy member in declaration order.
    Failover,
}

// --------------------------------------------------------------------- motd

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Motd {
    /// When false, status pings are proxied to the backend instead of being
    /// answered here.
    pub enabled: bool,
    pub text: String,
    pub version_name: String,
    pub protocol: ProtocolPolicy,
    pub max_players: i64,
    pub players: PlayerSource,
    /// Used when `players: static`.
    pub online: i64,
    /// Extra lines shown when hovering the player count.
    pub sample: Vec<String>,
    /// PNG file, 64x64. Loaded once at start-up.
    pub favicon: Option<PathBuf>,
    pub enforces_secure_chat: Option<bool>,
    /// Shown when the route has no healthy backend.
    pub offline: OfflineMotd,
}

impl Default for Motd {
    fn default() -> Self {
        Self {
            enabled: true,
            text: "A Minecraft Network".into(),
            version_name: "Network".into(),
            protocol: ProtocolPolicy::Auto,
            max_players: 1000,
            players: PlayerSource::Sessions,
            online: 0,
            sample: Vec::new(),
            favicon: None,
            enforces_secure_chat: Some(false),
            offline: OfflineMotd::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OfflineMotd {
    pub text: String,
    /// Report 0 players and a protocol of -1, which renders as "offline" in the
    /// client's server list.
    pub mark_incompatible: bool,
}

impl Default for OfflineMotd {
    fn default() -> Self {
        Self { text: "&cCurrently offline".into(), mark_incompatible: true }
    }
}

/// Per-route overlay. Absent fields fall back to the global MOTD.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MotdOverride {
    pub enabled: Option<bool>,
    pub text: Option<String>,
    pub version_name: Option<String>,
    pub protocol: Option<ProtocolPolicy>,
    pub max_players: Option<i64>,
    pub players: Option<PlayerSource>,
    pub online: Option<i64>,
    pub sample: Option<Vec<String>>,
    pub favicon: Option<PathBuf>,
    pub enforces_secure_chat: Option<bool>,
    pub offline: Option<OfflineMotd>,
}

impl Motd {
    /// Applies a route overlay, returning the effective MOTD.
    pub fn overlay(&self, over: Option<&MotdOverride>) -> Motd {
        let Some(over) = over else { return self.clone() };
        Motd {
            enabled: over.enabled.unwrap_or(self.enabled),
            text: over.text.clone().unwrap_or_else(|| self.text.clone()),
            version_name: over.version_name.clone().unwrap_or_else(|| self.version_name.clone()),
            protocol: over.protocol.unwrap_or(self.protocol),
            max_players: over.max_players.unwrap_or(self.max_players),
            players: over.players.unwrap_or(self.players),
            online: over.online.unwrap_or(self.online),
            sample: over.sample.clone().unwrap_or_else(|| self.sample.clone()),
            favicon: over.favicon.clone().or_else(|| self.favicon.clone()),
            enforces_secure_chat: over.enforces_secure_chat.or(self.enforces_secure_chat),
            offline: over.offline.clone().unwrap_or_else(|| self.offline.clone()),
        }
    }
}

/// What to report as the server's protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolPolicy {
    /// Echo the client's own protocol number, so the server list never shows
    /// the red "incompatible version" cross regardless of what the client runs.
    Auto,
    /// A fixed number, which makes mismatching clients show as incompatible.
    Fixed(i32),
}

impl ProtocolPolicy {
    pub fn resolve(self, client_protocol: i32) -> i32 {
        match self {
            ProtocolPolicy::Auto => client_protocol,
            ProtocolPolicy::Fixed(value) => value,
        }
    }
}

impl<'de> Deserialize<'de> for ProtocolPolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl de::Visitor<'_> for Visitor {
            type Value = ProtocolPolicy;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("`auto` or a protocol number")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.eq_ignore_ascii_case("auto") {
                    Ok(ProtocolPolicy::Auto)
                } else {
                    value
                        .parse()
                        .map(ProtocolPolicy::Fixed)
                        .map_err(|_| E::custom(format!("expected `auto` or a number, got `{value}`")))
                }
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                i32::try_from(value)
                    .map(ProtocolPolicy::Fixed)
                    .map_err(|_| E::custom("protocol number out of range"))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                i32::try_from(value)
                    .map(ProtocolPolicy::Fixed)
                    .map_err(|_| E::custom("protocol number out of range"))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl Serialize for ProtocolPolicy {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ProtocolPolicy::Auto => serializer.serialize_str("auto"),
            ProtocolPolicy::Fixed(value) => serializer.serialize_i32(*value),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerSource {
    /// Sessions currently proxied by this gateway.
    #[default]
    Sessions,
    /// The fixed `online` value.
    Static,
    /// Sum of the player counts seen by status health checks.
    Backends,
}

// ----------------------------------------------------------------- messages

/// Disconnect reasons. Shown to the player, so they are config, not constants.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Messages {
    pub no_route: String,
    pub backend_offline: String,
    pub backend_error: String,
    pub rate_limited: String,
    pub too_many_connections: String,
}

impl Default for Messages {
    fn default() -> Self {
        Self {
            no_route: "&cUnknown server address".into(),
            backend_offline: "&cThis server is currently offline".into(),
            backend_error: "&cCould not reach the server, please try again".into(),
            rate_limited: "&cToo many connection attempts, slow down".into(),
            too_many_connections: "&cThe network is full, please try again later".into(),
        }
    }
}

// ------------------------------------------------------------------- limits

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Total simultaneous client connections; 0 means unlimited.
    pub max_connections: usize,
    /// Simultaneous connections from a single IP; 0 means unlimited.
    pub max_connections_per_ip: usize,
    /// Token bucket for new connections per IP.
    pub connection_rate: RateLimit,
    /// Bytes read before the handshake is understood. A client that sends more
    /// than this without producing a valid handshake is dropped.
    pub max_handshake_bytes: usize,
    /// Exempt these networks from every per-IP limit (monitoring, your own LAN).
    pub exempt: IpNets,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 20_000,
            max_connections_per_ip: 8,
            connection_rate: RateLimit::default(),
            max_handshake_bytes: 8 * 1024,
            exempt: IpNets::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimit {
    /// Connections allowed to burst before the bucket empties; 0 disables.
    pub burst: u32,
    /// Time to refill the bucket completely.
    pub per: HumanDuration,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self { burst: 30, per: HumanDuration::from_secs(60) }
    }
}

impl RateLimit {
    pub fn is_disabled(&self) -> bool {
        self.burst == 0 || self.per.is_zero()
    }
}

// ----------------------------------------------------------------- timeouts

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    /// Time allowed to deliver a complete handshake after connecting.
    pub handshake: HumanDuration,
    /// Time allowed for the whole status exchange.
    pub status: HumanDuration,
    /// TCP connect to the backend.
    pub connect: HumanDuration,
    /// Idle time on an established session before it is closed; 0 disables.
    pub idle: HumanDuration,
    /// Grace period for draining sessions on shutdown.
    pub drain: HumanDuration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            handshake: HumanDuration::from_secs(5),
            status: HumanDuration::from_secs(10),
            connect: HumanDuration::from_secs(3),
            idle: HumanDuration::from_secs(600),
            drain: HumanDuration::from_secs(30),
        }
    }
}

// ------------------------------------------------------------------- health

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Health {
    pub enabled: bool,
    pub method: HealthMethod,
    pub interval: HumanDuration,
    pub timeout: HumanDuration,
    /// Consecutive successes before a down server is used again.
    pub rise: u32,
    /// Consecutive failures before a server is taken out.
    pub fall: u32,
    /// Assume servers are up until proven otherwise, so a restart of the
    /// gateway does not reject players during the first check interval.
    pub start_healthy: bool,
}

impl Default for Health {
    fn default() -> Self {
        Self {
            enabled: true,
            method: HealthMethod::Status,
            interval: HumanDuration::from_secs(10),
            timeout: HumanDuration::from_secs(2),
            rise: 2,
            fall: 3,
            start_healthy: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthMethod {
    /// TCP connect only.
    Tcp,
    /// Full status ping, which also yields player counts and the backend's
    /// version string.
    #[default]
    Status,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthOverride {
    pub enabled: Option<bool>,
    pub method: Option<HealthMethod>,
    pub interval: Option<HumanDuration>,
    pub timeout: Option<HumanDuration>,
    pub rise: Option<u32>,
    pub fall: Option<u32>,
}

impl Health {
    pub fn overlay(&self, over: Option<&HealthOverride>) -> Health {
        let Some(over) = over else { return *self };
        Health {
            enabled: over.enabled.unwrap_or(self.enabled),
            method: over.method.unwrap_or(self.method),
            interval: over.interval.unwrap_or(self.interval),
            timeout: over.timeout.unwrap_or(self.timeout),
            rise: over.rise.unwrap_or(self.rise),
            fall: over.fall.unwrap_or(self.fall),
            start_healthy: self.start_healthy,
        }
    }
}

// ------------------------------------------------------ metrics and logging

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Metrics {
    pub enabled: bool,
    /// Prometheus scrape endpoint. Keep it off the public interface.
    pub bind: SocketAddr,
}

impl Default for Metrics {
    fn default() -> Self {
        Self { enabled: false, bind: "127.0.0.1:9100".parse().expect("valid default") }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Log {
    pub level: String,
    pub format: LogFormat,
    /// Include client IPs in connection logs. Turning this off keeps the
    /// gateway useful while logging no personal data.
    pub client_ip: bool,
}

impl Default for Log {
    fn default() -> Self {
        Self { level: "info".into(), format: LogFormat::Text, client_ip: true }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Text,
    Json,
}
