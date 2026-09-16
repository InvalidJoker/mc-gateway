//! Process-wide state and the reload path.
//!
//! Everything that a config reload can change lives behind one atomic pointer.
//! Sessions take a snapshot when they start and keep using it until they end,
//! so a reload never mutates a connection out from under itself.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use arc_swap::{ArcSwap, Guard};
use mc_config::{Config, InboundProxyProtocol, Listener, Loaded};
use mc_routing::{Registry, health};
use tokio::{sync::watch, task::JoinHandle};
use tracing::info;

use crate::limits::Limiter;

/// The swappable half of the gateway's state.
#[derive(Debug)]
pub struct Runtime {
    pub registry: Arc<Registry>,
}

/// A bound listener's identity, handed to every session it accepts.
#[derive(Debug, Clone)]
pub struct ListenerRuntime {
    pub name: String,
    pub bind: SocketAddr,
    pub proxy_protocol: InboundProxyProtocol,
}

impl From<&Listener> for ListenerRuntime {
    fn from(listener: &Listener) -> Self {
        Self {
            name: listener.name.clone(),
            bind: listener.bind,
            proxy_protocol: listener.proxy_protocol.clone(),
        }
    }
}

pub struct App {
    pub runtime: Arc<ArcSwap<Runtime>>,
    pub limiter: Arc<Limiter>,
    pub config_path: PathBuf,
    log_client_ip: AtomicBool,
    health: Mutex<Option<HealthTask>>,
    shutdown: watch::Sender<bool>,
}

struct HealthTask {
    stop: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

impl App {
    pub fn start(loaded: Loaded, config_path: PathBuf) -> Arc<Self> {
        let config = Arc::new(loaded.config);
        let registry = Registry::build(Arc::clone(&config));

        let (shutdown, _) = watch::channel(false);
        let app = Arc::new(Self {
            runtime: Arc::new(ArcSwap::from_pointee(Runtime {
                registry: Arc::clone(&registry),
            })),
            limiter: Limiter::new(config.limits.clone()),
            config_path,
            log_client_ip: AtomicBool::new(config.log.client_ip),
            health: Mutex::new(None),
            shutdown,
        });

        app.restart_health_checks(registry);
        app
    }

    pub fn runtime(&self) -> Guard<Arc<Runtime>> {
        self.runtime.load()
    }

    pub fn config(&self) -> Arc<Config> {
        Arc::clone(self.runtime().registry.config())
    }

    pub fn shutdown_signal(&self) -> watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    pub fn begin_shutdown(&self) {
        let _ = self.shutdown.send(true);
        if let Some(task) = self.health.lock().expect("health lock").take() {
            let _ = task.stop.send(true);
            task.handle.abort();
        }
    }

    /// Renders a client address for logs, honouring `log.client_ip`.
    pub fn log_addr(&self, addr: SocketAddr) -> String {
        if self.log_client_ip.load(Ordering::Relaxed) {
            addr.to_string()
        } else {
            "redacted".to_owned()
        }
    }

    /// Re-reads the config file and swaps in the new routing state.
    ///
    /// Listener changes are reported but not applied: rebinding sockets would
    /// drop connections, which is the one thing a reload must not do.
    pub async fn reload(&self) -> Result<Vec<String>, String> {
        let loaded = Config::load(&self.config_path).map_err(|err| err.to_string())?;
        let mut warnings = loaded.warnings;
        let config = Arc::new(loaded.config);

        let previous = self.runtime();
        if listeners_changed(previous.registry.config(), &config) {
            warnings.push(
                "listener definitions changed; restart the gateway to apply them".to_owned(),
            );
        }

        let registry =
            Registry::build_inheriting(Arc::clone(&config), Some(&previous.registry));

        self.limiter.set_limits(config.limits.clone());
        self.log_client_ip.store(config.log.client_ip, Ordering::Relaxed);
        self.runtime.store(Arc::new(Runtime { registry: Arc::clone(&registry) }));
        self.restart_health_checks(registry);

        info!(
            servers = config.servers.len(),
            routes = config.routing.rules.len(),
            "configuration reloaded"
        );
        Ok(warnings)
    }

    fn restart_health_checks(&self, registry: Arc<Registry>) {
        let (stop, rx) = watch::channel(false);
        let handle = tokio::spawn(health::run(registry, rx));

        let previous = self.health.lock().expect("health lock").replace(HealthTask { stop, handle });
        if let Some(previous) = previous {
            // Tell the old checkers to stop, then make sure they do.
            let _ = previous.stop.send(true);
            previous.handle.abort();
        }
    }
}

fn listeners_changed(old: &Config, new: &Config) -> bool {
    let key = |config: &Config| {
        config
            .listeners
            .iter()
            .map(|l| (l.name.clone(), l.bind, l.backlog))
            .collect::<Vec<_>>()
    };
    key(old) != key(new)
}
