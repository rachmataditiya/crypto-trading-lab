mod config;
mod data_feed;
mod indicators;
mod journal;
mod models;
mod realtime;
mod regime;
mod risk;
mod runner;
mod sanitize;
mod simulator;
mod strategy;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Paper trading simulation engine for crypto.
/// SAFETY: This engine operates in paper-only mode.
/// No live orders, no exchange API calls, no wallet access.
#[derive(Parser)]
#[command(name = "sim-engine", version, about)]
struct Cli {
    /// Workspace root directory (default: auto-detect from binary location)
    #[arg(long, env = "OPENCLAW_WORKSPACE")]
    workspace: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a single simulation cycle
    Once,

    /// Run as a daemon, repeating every N minutes
    Daemon {
        /// Interval between cycles in minutes
        #[arg(long, default_value = "60")]
        interval_minutes: u64,
    },

    /// Backfill simulation for the last N hours
    Backfill {
        /// Number of hours to backfill
        #[arg(long, default_value = "6")]
        hours: usize,
    },

    /// Run realtime market-data daemon (OKX public, read-only)
    RealtimeDaemon,
}

fn main() {
    let cli = Cli::parse();

    let workspace = cli.workspace.unwrap_or_else(|| {
        // Walk up from binary to find workspace root
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        // engine binary is at skills/crypto-trading-lab/engine/target/...
        // workspace is 4 levels up
        let mut path = exe.clone();
        for _ in 0..6 {
            path = path.parent().unwrap_or(&path).to_path_buf();
        }
        // Fallback: check if data/trading-journal exists
        if path.join("data/trading-journal").exists() {
            path
        } else {
            // Try current working directory
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            if cwd.join("data/trading-journal").exists() {
                cwd
            } else {
                eprintln!(
                    "[WARN] Could not auto-detect workspace. Use --workspace or set OPENCLAW_WORKSPACE."
                );
                cwd
            }
        }
    });

    eprintln!("[SIM] Workspace: {}", workspace.display());
    eprintln!("[SIM] PAPER-ONLY MODE — no live orders will be placed");

    let result = match cli.command {
        Commands::Once => runner::run_once(&workspace),
        Commands::Daemon { interval_minutes } => runner::run_daemon(&workspace, interval_minutes),
        Commands::Backfill { hours } => runner::run_backfill(&workspace, hours),
        Commands::RealtimeDaemon => {
            let cfg = match config::AccountConfig::load(&workspace) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[FATAL] {}", e);
                    std::process::exit(1);
                }
            };
            if !cfg.realtime.enabled {
                Err("realtime.enabled=false in account_config.json".to_string())
            } else {
                eprintln!(
                    "[REALTIME] starting OKX public read-only daemon for {:?}",
                    cfg.realtime.symbols
                );
                realtime::run_daemon(&workspace, &cfg.realtime)
            }
        }
    };

    if let Err(e) = result {
        eprintln!("[FATAL] {}", e);
        std::process::exit(1);
    }
}
