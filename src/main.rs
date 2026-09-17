use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use mc_gateway::{
    config::{Config, LogFormat},
    netsetup, server,
};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
#[command(
    name = "mc-gateway",
    version,
    about = "Adds a line to the MOTD of every Minecraft server on a node"
)]
struct Args {
    /// Path to the configuration file.
    #[arg(
        short,
        long,
        default_value = "/etc/mc-gateway/config.yaml",
        env = "MC_GATEWAY_CONFIG"
    )]
    config: PathBuf,

    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,

    /// Print the script that installs the firewall and routing rules, then exit.
    #[arg(long)]
    print_network_setup: bool,

    /// Print the script that removes them again, then exit.
    #[arg(long)]
    print_network_teardown: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();

    // Removing the rules must work even when the config is gone or broken.
    if args.print_network_teardown {
        print!("{}", netsetup::teardown_script());
        return ExitCode::SUCCESS;
    }

    let loaded = match Config::load(&args.config) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("mc-gateway: {err}");
            return ExitCode::FAILURE;
        }
    };

    init_logging(&loaded.config);
    for warning in &loaded.warnings {
        warn!("{warning}");
    }

    // Scripts go to stdout on their own, so they can be piped into a shell.
    if args.print_network_setup {
        print!("{}", netsetup::setup_script(&loaded.config));
        return ExitCode::SUCCESS;
    }
    if args.check {
        info!(path = %args.config.display(), "configuration is valid");
        return ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            error!(%err, "cannot start the async runtime");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(async {
        let running = match server::start(loaded, args.config).await {
            Ok(running) => running,
            Err(err) => {
                error!("{err}");
                return ExitCode::FAILURE;
            }
        };
        info!(version = env!("CARGO_PKG_VERSION"), "mc-gateway started");
        wait_for_signal(&running.gateway).await;
        running.shutdown().await;
        ExitCode::SUCCESS
    })
}

/// Waits for SIGINT or SIGTERM, reloading the config on SIGHUP.
#[cfg(unix)]
async fn wait_for_signal(gateway: &server::Gateway) {
    use tokio::signal::unix::{SignalKind, signal};

    let (Ok(mut interrupt), Ok(mut terminate), Ok(mut hangup)) = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        error!("cannot listen for signals");
        return;
    };

    loop {
        tokio::select! {
            _ = interrupt.recv() => return,
            _ = terminate.recv() => return,
            _ = hangup.recv() => match gateway.reload() {
                Ok(warnings) => warnings.iter().for_each(|w| warn!("{w}")),
                Err(err) => error!(%err, "reload failed; keeping the running configuration"),
            },
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_signal(_gateway: &server::Gateway) {
    let _ = tokio::signal::ctrl_c().await;
}

fn init_logging(config: &Config) {
    let level = &config.log.level;
    // RUST_LOG wins, so the level can be raised without editing the config.
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new(format!("mc_gateway={level},warn")));
    // stderr, so stdout stays clean for the printed scripts.
    let builder = fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr);
    match config.log.format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.init(),
    }
}
