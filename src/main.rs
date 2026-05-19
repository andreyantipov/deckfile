//! deckfile — declarative Stream Deck controller.
//!
//! Subcommands:
//!   deckfile run [-f PATH] [-d]     run the daemon (reads deckfile.yaml)
//!   deckfile validate [PATH]        parse-check without touching hardware
//!   deckfile devices                list connected Stream Deck devices
//!
//! Future: `deckfile mcp` — MCP server letting LLM agents edit deckfile.yaml.

mod config;
mod daemon;
mod render;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon: listen to Stream Deck, dispatch shell commands
    Run {
        /// Path to deckfile.yaml.
        /// Lookup if omitted: $DECKFILE → ./deckfile.yaml → $XDG_CONFIG_HOME/deckfile/deckfile.yaml
        #[arg(short = 'f', long = "config")]
        config: Option<PathBuf>,

        /// Daemonize: detach from the terminal (fork + setsid + close stdio).
        /// Not needed under systemd (`Type=simple`).
        #[arg(short = 'd', long)]
        daemonize: bool,
    },
    /// Parse-check the deckfile.yaml without connecting to a device
    Validate {
        path: Option<PathBuf>,
    },
    /// List connected Stream Deck devices
    Devices,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "deckfile=info".into())
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run { config, daemonize } => {
            if daemonize {
                detach().context("daemonize")?;
            }
            daemon::run(config)
        }
        Cmd::Validate { path } => validate(path),
        Cmd::Devices => devices(),
    }
}

/// Detach from controlling terminal. Equivalent to `daemon(nochdir=1, noclose=0)`:
/// fork once → setsid → fork again so we can't reacquire a controlling terminal →
/// redirect stdio to /dev/null. Logs (tracing) still write to /dev/null after this,
/// so set RUST_LOG=… and run under a process supervisor for visibility, or use
/// `systemd --user` instead.
fn detach() -> Result<()> {
    // daemon(nochdir=true, noclose=false): fork + setsid + redirect stdio
    // to /dev/null. Don't chdir to / so the daemon inherits the current
    // working directory (matters for relative deckfile.yaml paths).
    nix::unistd::daemon(true, false)
        .map_err(|e| anyhow::anyhow!("daemon(): {e}"))?;
    Ok(())
}

fn validate(path: Option<PathBuf>) -> Result<()> {
    if let Some(p) = path {
        std::env::set_var("DECKFILE", p);
    }
    let cfg = config::Deckfile::load()?;
    println!("✓ valid deckfile.yaml");
    println!("  brightness: {}", cfg.device.brightness);
    println!("  pages:      {}", cfg.pages.len());
    for (name, page) in &cfg.pages {
        println!("    {}: {} buttons, {} dials",
            name, page.buttons.len(), page.dials.len());
    }
    Ok(())
}

fn devices() -> Result<()> {
    let hid = elgato_streamdeck::new_hidapi()?;
    let devs = elgato_streamdeck::list_devices(&hid);
    if devs.is_empty() {
        println!("(no Stream Deck detected)");
        return Ok(());
    }
    for (kind, serial) in devs {
        println!("{:?}  serial={}", kind, serial);
    }
    Ok(())
}
