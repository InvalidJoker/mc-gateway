//! The configuration model.
//!
//! Every section has a `Default` that is safe to run with, so a minimal file
//! only needs a listener, a route and a server. Unknown keys are rejected so a
//! typo fails at start-up instead of silently changing behaviour in production.

use std::{collections::BTreeMap, fmt, net::SocketAddr, time::Duration};

use serde::{Deserialize, Serialize};

use crate::net::IpNets;

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
    /// Target used when no rule matches, and for clients that never send a
    /// hostname at all (pre-1.7 pings). Without it, those are refused.
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
    /// Per-route MOTD lines, overriding the global ones.
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
    /// Metadata for logs and metrics labels. It changes no behaviour.
    #[serde(default)]
    pub kind: BackendKind,
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
}

impl fmt::Display for BackendKind {
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

/// The gateway proxies status pings to the backend and passes the response
/// through untouched — version, player counts, sample and favicon all come from
/// the server that is actually running.
///
/// The only thing it changes is the MOTD text, and only the lines named here.
/// Leaving both unset means the status response is forwarded byte for byte.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Motd {
    /// Replaces the first line of the backend's MOTD. Unset keeps it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line1: Option<String>,
    /// Replaces the second line, which is where a network usually puts its
    /// own branding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line2: Option<String>,
    /// Shown when there is no reachable backend to ask.
    pub offline: OfflineMotd,
}

impl Motd {
    /// Whether the status response has to be parsed at all.
    pub fn rewrites_anything(&self) -> bool {
        self.line1.is_some() || self.line2.is_some()
    }
}

/// The one case the gateway cannot pass through: there is no backend to ask.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OfflineMotd {
    pub text: String,
    pub version_name: String,
    pub max_players: i64,
    /// Report protocol -1, which renders the entry as incompatible — the
    /// clearest way to say "the gateway is up, that server is not".
    pub mark_incompatible: bool,
}

impl Default for OfflineMotd {
    fn default() -> Self {
        Self {
            text: "&cCurrently offline".into(),
            version_name: "offline".into(),
            max_players: 0,
            mark_incompatible: true,
        }
    }
}

/// Per-route overlay on top of the global MOTD.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MotdOverride {
    pub line1: Option<String>,
    pub line2: Option<String>,
    pub offline: Option<OfflineMotd>,
}

impl Motd {
    pub fn overlay(&self, over: Option<&MotdOverride>) -> Motd {
        let Some(over) = over else { return self.clone() };
        Motd {
            line1: over.line1.clone().or_else(|| self.line1.clone()),
            line2: over.line2.clone().or_else(|| self.line2.clone()),
            offline: over.offline.clone().unwrap_or_else(|| self.offline.clone()),
        }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimit {
    /// Connections allowed to burst before the bucket empties; 0 disables.
    pub burst: u32,
    /// Time to refill the bucket completely.
    #[serde(with = "humantime_serde")]
    pub per: Duration,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self { burst: 30, per: Duration::from_secs(60) }
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
    #[serde(with = "humantime_serde")]
    pub handshake: Duration,
    /// Time allowed for the backend to answer a proxied status ping.
    #[serde(with = "humantime_serde")]
    pub status: Duration,
    /// TCP connect to the backend.
    #[serde(with = "humantime_serde")]
    pub connect: Duration,
    /// Idle time on an established session before it is closed; 0 disables.
    #[serde(with = "humantime_serde")]
    pub idle: Duration,
    /// Grace period for draining sessions on shutdown.
    #[serde(with = "humantime_serde")]
    pub drain: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(5),
            status: Duration::from_secs(10),
            connect: Duration::from_secs(3),
            idle: Duration::from_secs(600),
            drain: Duration::from_secs(30),
        }
    }
}

// ------------------------------------------------------------------- health

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Health {
    pub enabled: bool,
    pub method: HealthMethod,
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
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
            interval: Duration::from_secs(10),
            timeout: Duration::from_secs(2),
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
    /// Full status ping, which also yields player counts and the version.
    #[default]
    Status,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HealthOverride {
    pub enabled: Option<bool>,
    pub method: Option<HealthMethod>,
    #[serde(default, with = "humantime_serde")]
    pub interval: Option<Duration>,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Option<Duration>,
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
