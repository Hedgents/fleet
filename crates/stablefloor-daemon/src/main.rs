//! Stable-floor daemon — single-shot: mint or redeem Sanctum INF, exit.
//!
//! TODO(strategy plan): post a pre-signed envelope to the co-located
//! node-enterprise instance at --peer-node-api before/after the
//! mint/redeem so the orchestrator can track the operation. Currently
//! the daemon only loads its role identity for forward-compat; the
//! actual envelope POST is deferred.

mod sanctum;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;
use zerox1_defi_runtime::{
    build_runtime, RuntimeProfile,
    identity::Role,
    secrets::{FileSource, load_role_identity},
};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_WALLET")]
    wallet: PathBuf,

    /// Directory holding the daemon's role-key file (and any other secrets).
    #[arg(long, env = "ZX_SECRETS_DIR")]
    secrets_dir: PathBuf,

    #[arg(long, env = "ZX_STAMP", default_value = "stablefloor.last")]
    stamp: PathBuf,

    /// Co-located node-enterprise instance's local HTTP API base URL.
    /// The daemon posts pre-signed envelopes here. Currently unused at the
    /// scaffold stage; the strategy plan wires the actual POST /hosted/send
    /// call once the strategy logic is in place.
    #[arg(long, env = "ZX_PEER_NODE_API", default_value = "http://127.0.0.1:8080")]
    peer_node_api: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Mint   { #[arg(long)] sol_amount: f64 },
    Redeem { #[arg(long)] inf_amount: f64 },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let wallet = Wallet::load(&args.wallet)?;
    let whitelist = SigningWhitelist::new(sanctum::program_ids());
    let rt = build_runtime(RuntimeProfile::OneShot)?;
    rt.block_on(async move {
        // Load the role identity (separate from the Solana wallet) so the
        // daemon's mesh identity is portable across machines. The strategy
        // follow-up plan wires this into envelope-signed calls to the
        // co-located node-enterprise instance at args.peer_node_api.
        let secrets = FileSource::new(&args.secrets_dir);
        let role_identity = load_role_identity(&secrets, Role::StableFloor, "stablefloor-role.key").await?;
        info!(role = %role_identity.role().as_str(), peer_api = %args.peer_node_api, "stablefloor role identity loaded");
        let _ = role_identity;  // unused for now — placeholder for the strategy plan

        match args.cmd {
            Cmd::Mint   { sol_amount } => sanctum::mint(&wallet, &whitelist, sol_amount).await?,
            Cmd::Redeem { inf_amount } => sanctum::redeem(&wallet, &whitelist, inf_amount).await?,
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        std::fs::write(&args.stamp, now.to_string())
            .with_context(|| format!("write stablefloor stamp at {}", args.stamp.display()))?;
        info!("stablefloor done");
        Ok::<_, anyhow::Error>(())
    })
}
