//! Hedgents fleet-dashboard-server — local dashboard backend.
//!
//! Tails daemon JSON tracing logs + per-daemon `*-pnl.jsonl` files,
//! decodes signed-envelope events into human-readable mesh events,
//! persists everything to a local SQLite, and serves a REST + WebSocket
//! API on `127.0.0.1:7700` for the dashboard frontend.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::{broadcast, mpsc};
use tracing::{info, warn};

use fleet_dashboard_server::api::{self, AppState};
use fleet_dashboard_server::chain::ChainReader;
use fleet_dashboard_server::ingest::{apr_sampler, envelope_decoder, log_tailer, pnl_jsonl};
use fleet_dashboard_server::store::Store;
use fleet_dashboard_server::types::{MeshEvent, RawLogLine};

#[derive(Parser, Debug)]
#[command(
    name = "fleet-dashboard-server",
    about = "Hedgents fleet local dashboard backend"
)]
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

    /// REST + WS bind address.
    #[arg(long, default_value = "127.0.0.1:7700")]
    listen: String,

    /// Path to operator's Solana wallet keypair JSON. Pubkey is read
    /// only — this server never signs.
    #[arg(long)]
    solana_wallet: PathBuf,

    /// Solana RPC endpoint. Defaults to mainnet-beta.
    #[arg(long, default_value = "https://api.mainnet-beta.solana.com")]
    rpc_url: String,
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
    let chain = Arc::new(ChainReader::new(args.rpc_url.clone()));
    let wallet_pubkey = parse_wallet_pubkey(&args.solana_wallet).with_context(|| {
        format!(
            "loading wallet pubkey from {}",
            args.solana_wallet.display()
        )
    })?;

    let (event_broadcast_tx, _) = broadcast::channel::<MeshEvent>(1024);

    // log_tailer -> decoder loop.
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

    // Background APR sampler — snapshots live APR every 60s for /apr/history.
    let store_for_apr = store.clone();
    let chain_for_apr = chain.clone();
    tokio::spawn(async move {
        apr_sampler::run(store_for_apr, chain_for_apr).await;
    });

    let store_for_decoder = store.clone();
    let broadcast_tx_clone = event_broadcast_tx.clone();
    tokio::spawn(async move {
        while let Some(raw) = log_rx.recv().await {
            if let Some(event) = envelope_decoder::decode_log_line(&raw) {
                match store_for_decoder.insert_mesh_event(&event).await {
                    Ok(id) => {
                        let mut e_with_id = event.clone();
                        e_with_id.id = Some(id);
                        let _ = broadcast_tx_clone.send(e_with_id);
                    }
                    Err(e) => warn!(?e, "insert_mesh_event failed"),
                }
            }
        }
    });

    let app_state = AppState {
        store: store.clone(),
        chain: chain.clone(),
        event_broadcast: event_broadcast_tx.clone(),
        wallet_pubkey,
        rpc_url: args.rpc_url.clone(),
    };

    let app = api::router(app_state);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    info!(listen = %args.listen, "dashboard API listening");
    axum::serve(listener, app).await?;

    Ok(())
}

/// Load the public key from a Solana keypair JSON file (the standard
/// `solana-keygen` byte-array format). We only need the pubkey here —
/// this server never signs anything.
fn parse_wallet_pubkey(path: &PathBuf) -> Result<Pubkey> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw)
        .with_context(|| format!("parsing keypair JSON at {}", path.display()))?;
    if bytes.len() != 64 {
        anyhow::bail!(
            "keypair JSON at {} has {} bytes, expected 64",
            path.display(),
            bytes.len()
        );
    }
    // Last 32 bytes of an Ed25519 keypair are the public key.
    let pubkey_bytes: [u8; 32] = bytes[32..64]
        .try_into()
        .context("slicing pubkey out of keypair")?;
    Ok(Pubkey::new_from_array(pubkey_bytes))
}
