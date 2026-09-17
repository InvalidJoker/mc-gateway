//! Starting, reloading and stopping the gateway.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use arc_swap::ArcSwap;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time,
};
use tracing::{info, warn};

use crate::{
    config::{Config, Loaded},
    intercept, observe, transparent,
};

/// State shared by every connection. Connections read the config once when
/// they start, so a reload never changes a connection halfway through.
pub struct Gateway {
    config: ArcSwap<Config>,
    config_path: PathBuf,
    shutdown: watch::Sender<bool>,
}

impl Gateway {
    pub fn new(config: Config, config_path: PathBuf) -> Arc<Self> {
        let (shutdown, _) = watch::channel(false);
        Arc::new(Self { config: ArcSwap::from_pointee(config), config_path, shutdown })
    }

    pub fn config(&self) -> Arc<Config> {
        self.config.load_full()
    }

    /// Renders a player address for logs, honouring `log.client_ip`.
    pub fn log_addr(&self, addr: SocketAddr) -> String {
        if self.config.load().log.client_ip { addr.to_string() } else { "redacted".to_owned() }
    }

    /// Re-reads the config file. The MOTD lines, timeouts and log settings take
    /// effect for new connections; ports and listen addresses are baked into the
    /// firewall rules and need a restart.
    pub fn reload(&self) -> Result<Vec<String>, String> {
        let loaded = Config::load(&self.config_path).map_err(|err| err.to_string())?;
        let mut warnings = loaded.warnings;
        let current = self.config();
        if current.ports != loaded.config.ports
            || current.listen != loaded.config.listen
            || current.listen_v6 != loaded.config.listen_v6
        {
            warnings.push("ports or listen addresses changed; restart to apply them".into());
        }
        self.config.store(Arc::new(loaded.config));
        info!("configuration reloaded");
        Ok(warnings)
    }

    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }
}

/// A started gateway.
pub struct Running {
    pub gateway: Arc<Gateway>,
    listeners: Vec<JoinHandle<()>>,
    /// Every connection holds a clone; the channel closes when all are gone.
    sessions: Option<mpsc::Sender<()>>,
    sessions_done: mpsc::Receiver<()>,
    drain: Duration,
}

/// Binds the listeners and starts intercepting.
pub async fn start(loaded: Loaded, config_path: PathBuf) -> Result<Running, String> {
    // Missing CAP_NET_ADMIN is a start-up failure, not something the first
    // player discovers.
    transparent::preflight().map_err(|err| {
        format!(
            "{err}\n\nThe gateway needs CAP_NET_ADMIN. Grant it with one of:\n  \
             sudo setcap cap_net_admin+ep <path to mc-gateway>\n  \
             systemd: AmbientCapabilities=CAP_NET_ADMIN\n  \
             docker: cap_add: [NET_ADMIN]"
        )
    })?;

    let config = loaded.config;
    let mut sockets = vec![(
        config.listen,
        transparent::listen(config.listen, 4096)
            .map_err(|err| format!("cannot listen on {}: {err}", config.listen))?,
    )];
    if let Some(listen_v6) = config.listen_v6 {
        // A host without IPv6 is not an error: IPv4 interception works on its
        // own, and IPv6 connections simply pass through.
        match transparent::listen(listen_v6, 4096) {
            Ok(socket) => sockets.push((listen_v6, socket)),
            Err(err) => warn!(address = %listen_v6, %err, "IPv6 interception unavailable"),
        }
    }

    if config.metrics.enabled {
        observe::install(config.metrics.bind)?;
        info!(bind = %config.metrics.bind, "metrics endpoint listening");
    }

    let drain = config.timeouts.drain;
    let gateway = Gateway::new(config, config_path);
    let (sessions, sessions_done) = mpsc::channel(1);
    let listeners = sockets
        .into_iter()
        .map(|(_, socket)| {
            tokio::spawn(intercept::run(
                socket,
                Arc::clone(&gateway),
                gateway.shutdown_signal(),
                sessions.clone(),
            ))
        })
        .collect();

    Ok(Running { gateway, listeners, sessions: Some(sessions), sessions_done, drain })
}

impl Running {
    /// Stops accepting, then waits for open connections up to `timeouts.drain`.
    ///
    /// Once the gateway stops listening, the firewall rules no longer match and
    /// new connections go straight to the servers. Connections already running
    /// through the gateway end when it exits.
    pub async fn shutdown(mut self) {
        info!("shutting down");
        let _ = self.gateway.shutdown.send(true);
        for task in self.listeners.drain(..) {
            let _ = task.await;
        }
        self.sessions.take();
        match time::timeout(self.drain, self.sessions_done.recv()).await {
            Ok(_) => info!("all connections closed"),
            Err(_) => warn!(drain = ?self.drain, "drain timed out; closing remaining connections"),
        }
    }
}
