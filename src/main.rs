//! deckfile — declarative Stream Deck controller.
//!
//! Subcommands:
//!   deckfile run [--config PATH]    run the daemon (reads deckfile.yaml)
//!   deckfile validate [PATH]        parse-check without touching hardware
//!   deckfile devices                list connected Stream Deck devices
//!
//! Future: `deckfile mcp` — MCP server letting LLM agents edit deckfile.yaml.

mod config;
mod daemon;
mod render;

use anyhow::Result;
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
        /// Path to deckfile.yaml (default: $DECKFILE → ./deckfile.yaml → $XDG_CONFIG_HOME/deckfile/deckfile.yaml)
        #[arg(long, short)]
        config: Option<PathBuf>,
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
        Cmd::Run { config } => daemon::run(config),
        Cmd::Validate { path } => validate(path),
        Cmd::Devices => devices(),
    }
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
