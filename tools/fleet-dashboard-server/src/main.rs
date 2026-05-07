//! Hedgents fleet-dashboard-server — local dashboard backend.
//!
//! Tails daemon JSON tracing logs + per-daemon `*-pnl.jsonl` files,
//! decodes signed-envelope events into human-readable mesh events, and
//! persists everything to a local SQLite. REST + WebSocket API on
//! `127.0.0.1:7700` lands Day 2 of the demo sprint.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tracing::{info, warn};

use fleet_dashboard_server::ingest::{envelope_decoder, log_tailer, pnl_jsonl};
use fleet_dashboard_server::store::Store;
use fleet_dashboard_server::types::RawLogLine;

#[derive(Parser, Debug)]
#[command(name = "fleet-dashboard-server", about = "Hedgents fleet local dashboard backend")]
struct Args {
    /// Directory containing daemon `*.log` JSON tracing files.
    #[arg(long, default_value = "./logs")]
    log_dir: PathBuf,

    /// Directory containing per-daemon `*-pnl.jsonl` (and
    /// `researcher-signals.jsonl`) telemetry files. Defaults to the same
    /// dir as `--log-dir`.
    #[arg(long, default_value = "./logs")]
    telemetry_dir: PathBuf,

    /// SQLite database path.
    #[arg(long, default_value = "./dashboard.sqlite")]
    db_path: PathBuf,

    /// REST + WS bind address (Day 2 — currently unused).
    #[arg(long, default_value = "127.0.0.1:7700")]
    listen: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    info!(?args, "fleet-dashboard-server starting");

    let store = Arc::new(Store::open(&args.db_path).await?);

    // Channel: log_tailer -> decoder loop.
    let (log_tx, mut log_rx) = mpsc::channel::<RawLogLine>(1024);

    let log_dir = args.log_dir.clone();
    let log_tx_clone = log_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = log_tailer::run(log_dir, log_tx_clone).await {
            warn!(?e, "log_tailer exited");
        }
    });

    let telemetry_dir = args.telemetry_dir.clone();
    let store_for_pnl = store.clone();
    tokio::spawn(async move {
        if let Err(e) = pnl_jsonl::run(telemetry_dir, store_for_pnl).await {
            warn!(?e, "pnl_jsonl exited");
        }
    });

    info!("Day 1 scope: ingest-only. REST/WS API lands Day 2.");
    info!(listen = %args.listen, "bind address reserved for Day 2");

    while let Some(raw) = log_rx.recv().await {
        if let Some(event) = envelope_decoder::decode_log_line(&raw) {
            if let Err(e) = store.insert_mesh_event(&event).await {
                warn!(?e, "insert_mesh_event failed");
            }
        }
    }

    Ok(())
}
