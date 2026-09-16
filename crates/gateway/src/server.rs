//! Starting and stopping the gateway as a unit.
//!
//! Kept separate from `main` so tests can run a real gateway — real sockets,
//! real sessions — instead of a stand-in.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use mc_config::Loaded;
use tokio::{sync::mpsc, task::JoinHandle, time};
use tracing::{info, warn};

use crate::{
    app::{App, ListenerRuntime},
    listener, observe,
};

/// A running gateway.
pub struct Server {
    pub app: Arc<App>,
    /// Bound addresses by listener name, with the real port when the config
    /// asked for port 0.
    addresses: Vec<(String, SocketAddr)>,
    listeners: Vec<JoinHandle<()>>,
    /// Kept alive so sessions can hold clones; dropping it starts the drain.
    sessions: Option<mpsc::Sender<()>>,
    sessions_done: mpsc::Receiver<()>,
    drain: Duration,
}

impl Server {
    /// Binds every listener, then starts accepting.
    ///
    /// Binding happens before anything else so that a port clash is a start-up
    /// failure rather than a half-running gateway.
    pub async fn start(loaded: Loaded, config_path: PathBuf) -> Result<Self, String> {
        // On Linux every backend connection is transparent, which needs
        // CAP_NET_ADMIN. Finding that out here beats finding it out on the
        // first player, where the symptom looks like a network fault.
        mc_forwarding::preflight()?;

        let mut bound = Vec::new();
        for listener in &loaded.config.listeners {
            let socket = listener::bind(listener.bind, listener.backlog).map_err(|err| {
                format!("cannot bind listener `{}` to {}: {err}", listener.name, listener.bind)
            })?;
            let address = socket.local_addr().map_err(|err| err.to_string())?;
            bound.push((Arc::new(ListenerRuntime::from(listener)), socket, address));
        }

        let metrics_config = loaded.config.metrics.clone();
        let drain = loaded.config.timeouts.drain;
        let app = App::start(loaded, config_path);

        let (sessions, sessions_done) = mpsc::channel::<()>(1);
        let mut addresses = Vec::new();
        let mut listeners = Vec::new();

        for (context, socket, address) in bound {
            addresses.push((context.name.clone(), address));
            listeners.push(tokio::spawn(listener::run(
                socket,
                context,
                Arc::clone(&app),
                app.shutdown_signal(),
                sessions.clone(),
            )));
        }

        if metrics_config.enabled {
            observe::install(metrics_config.bind)?;
            tokio::spawn(observe::run_gauges(
                Arc::clone(&app.runtime),
                app.shutdown_signal(),
            ));
        }

        info!(
            forwarding = %mc_forwarding::Mode::current(),
            "backend connections use this mode on this platform"
        );

        Ok(Self {
            app,
            addresses,
            listeners,
            sessions: Some(sessions),
            sessions_done,
            drain,
        })
    }

    /// Address a listener actually bound to.
    pub fn address(&self, listener: &str) -> Option<SocketAddr> {
        self.addresses.iter().find(|(name, _)| name == listener).map(|(_, addr)| *addr)
    }

    pub fn addresses(&self) -> &[(String, SocketAddr)] {
        &self.addresses
    }

    /// Stops accepting, then waits for open sessions up to the drain timeout.
    pub async fn shutdown(mut self) {
        info!("shutting down, draining sessions");
        self.app.begin_shutdown();

        for task in self.listeners.drain(..) {
            let _ = task.await;
        }

        // Once this sender is gone, the channel closes when the last session
        // task drops its clone.
        self.sessions.take();
        match time::timeout(self.drain, self.sessions_done.recv()).await {
            Ok(_) => info!("all sessions closed"),
            Err(_) => warn!(drain = ?self.drain, "drain timed out; closing remaining sessions"),
        }
    }
}
