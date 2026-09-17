//! mc-gateway — a Minecraft edge gateway.
//!
//! Either one public port in front of many backends, routed by the handshake
//! hostname, or — on a hosting node — an invisible layer in front of every
//! game server on a port range that adds a line to their MOTD.

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use clap::Parser;
use mc_config::{Config, LogFormat};
use mc_gateway::{app::App, server::Server};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(name = "mc-gateway", version, about = "Minecraft edge gateway")]
struct Args {
    /// Path to the configuration file.
    #[arg(short, long, default_value = "config.yaml", env = "MC_GATEWAY_CONFIG")]
    config: PathBuf,

    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,

    /// Print the configuration as the gateway understands it, then exit.
    #[arg(long)]
    dump: bool,

    /// Print the shell script that installs the firewall and routing rules
    /// this configuration needs, then exit. Pipe it to `sh` as root.
    #[arg(long)]
    print_network_setup: bool,

    /// Print the shell script that removes those rules again, then exit.
    #[arg(long)]
    print_network_teardown: bool,

    /// Override the log level (also honours RUST_LOG).
    #[arg(long)]
    log_level: Option<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();

    let loaded = match Config::load(&args.config) {
        Ok(loaded) => loaded,
        Err(err) => {
            // Logging is not up yet, and a config error is the one thing that
            // must be readable without it.
            eprintln!("mc-gateway: {err}");
            return ExitCode::FAILURE;
        }
    };

    init_logging(&loaded.config, args.log_level.as_deref());
    for warning in &loaded.warnings {
        warn!("{warning}");
    }

    // Only the script goes to stdout: it is meant to be piped into a shell.
    if args.print_network_setup {
        let interception = loaded.config.intercept.as_ref().map(|intercept| {
            mc_forwarding::netsetup::Interception {
                listen: intercept.listen,
                listen_v6: intercept.listen_v6,
                ports: intercept.ports.iter().map(|range| (range.start, range.end)).collect(),
            }
        });
        print!("{}", mc_forwarding::netsetup::setup_script(interception.as_ref()));
        return ExitCode::SUCCESS;
    }
    if args.print_network_teardown {
        print!("{}", mc_forwarding::netsetup::teardown_script());
        return ExitCode::SUCCESS;
    }

    if args.dump {
        match serde_yaml_ng::to_string(&loaded.config) {
            Ok(yaml) => println!("{yaml}"),
            Err(err) => {
                error!(%err, "cannot render the configuration");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }

    if args.check {
        info!(
            path = %args.config.display(),
            servers = loaded.config.servers.len(),
            routes = loaded.config.routing.rules.len(),
            warnings = loaded.warnings.len(),
            "configuration is valid"
        );
        return ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            error!(%err, "cannot start the async runtime");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(loaded, args.config)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("{err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(loaded: mc_config::Loaded, config_path: PathBuf) -> Result<(), String> {
    let server = Server::start(loaded, config_path).await?;
    let config = server.app.config();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        listeners = config.listeners.len(),
        servers = config.servers.len(),
        "mc-gateway started"
    );

    wait_for_signal(&server.app).await;
    server.shutdown().await;
    Ok(())
}

/// Waits for a termination signal, reloading on SIGHUP along the way.
#[cfg(unix)]
async fn wait_for_signal(app: &Arc<App>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut interrupt = match signal(SignalKind::interrupt()) {
        Ok(signal) => signal,
        Err(err) => {
            error!(%err, "cannot listen for SIGINT");
            return;
        }
    };
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(err) => {
            error!(%err, "cannot listen for SIGTERM");
            return;
        }
    };
    let mut hangup = match signal(SignalKind::hangup()) {
        Ok(signal) => signal,
        Err(err) => {
            error!(%err, "cannot listen for SIGHUP");
            return;
        }
    };

    loop {
        tokio::select! {
            _ = interrupt.recv() => return,
            _ = terminate.recv() => return,
            _ = hangup.recv() => {
                info!("SIGHUP received, reloading configuration");
                match app.reload().await {
                    Ok(warnings) => {
                        for warning in warnings {
                            warn!("{warning}");
                        }
                    }
                    // A bad config on reload leaves the running one in place.
                    Err(err) => error!(%err, "reload failed; keeping the running configuration"),
                }
            }
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signal(_app: &Arc<App>) {
    let _ = tokio::signal::ctrl_c().await;
}

fn init_logging(config: &Config, level_override: Option<&str>) {
    let level = level_override.unwrap_or(&config.log.level);
    // RUST_LOG wins, so an operator can raise the level without editing files.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("mc_gateway={level},mc_routing={level},mc_forwarding={level},warn")));

    // stderr, so that stdout stays clean for --print-network-setup and --dump.
    let builder = fmt().with_env_filter(filter).with_target(false).with_writer(std::io::stderr);
    match config.log.format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.init(),
    }
}
