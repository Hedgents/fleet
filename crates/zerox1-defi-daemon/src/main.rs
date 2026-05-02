//! zerox1-defi-daemon — localhost HTTP service for Solana DeFi operations.
//!
//! Binds to 127.0.0.1 only by default. Holds wallet keypair. Builds → signs →
//! broadcasts transactions through `zerox1-defi-protocols`. Optionally pairs
//! with a fleet orchestrator via `--fleet-id` + `--fleet-token` + `--role`.
//!
//! Any agent runtime (zeroclaw plugin, Claude Agent SDK script, raw curl) on
//! the same host can call the endpoints.

mod config;
mod handlers;
mod pairing;
mod persistence;
mod rpc;
mod server;
mod wallet;

use std::net::SocketAddr;

use anyhow::Result;
use clap::Parser;
use tracing::{info, warn};

use crate::config::Cli;
use crate::persistence::StateFile;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = cli.into_config()?;

    let wallet = wallet::Wallet::load(&cfg.wallet_path)?;
    info!(pubkey = %wallet.pubkey(), "loaded wallet");

    let rpc = rpc::RpcContext::new(cfg.rpc_url.clone(), cfg.commitment);
    info!(rpc = %cfg.rpc_url, "connected RPC");

    // Promote the partial fleet identity (CLI flags) by attaching the wallet
    // pubkey as the worker's agent_id. Same wallet is used for signing
    // Solana transactions and as the mesh identity.
    let fleet_identity = cfg.fleet_identity_partial.map(|p| p.complete(wallet.pubkey().to_string()));

    let state_file = StateFile::new(&cfg.data_dir);
    let initial_pairing = if fleet_identity.is_some() {
        match state_file.load() {
            Ok(s) => {
                info!(state = ?s, "loaded pairing state from disk");
                s
            }
            Err(e) => {
                warn!(?e, "could not load pairing state — starting Unpaired");
                pairing::PairingState::Unpaired
            }
        }
    } else {
        pairing::PairingState::Unpaired
    };

    if let Some(id) = &fleet_identity {
        info!(
            fleet_id = %hex::encode(id.fleet_id),
            role = ?id.role,
            topic = %id.discovery_topic(),
            "fleet identity configured"
        );
    } else {
        info!("fleet identity not configured (pairing endpoints will return 503)");
    }

    let state = server::AppState::new(rpc, wallet, fleet_identity, initial_pairing, state_file);

    let addr: SocketAddr = format!("{}:{}", cfg.bind_host, cfg.bind_port).parse()?;
    info!(%addr, "starting daemon");

    let app = server::router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
