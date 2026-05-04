//! Stable-floor daemon — single-shot: mint or redeem Sanctum INF, exit.

mod sanctum;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::info;
use zerox1_defi_runtime::{build_runtime, RuntimeProfile};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_WALLET")]    wallet: PathBuf,
    #[arg(long, env = "ZX_STAMP", default_value = "stablefloor.last")]
                                        stamp: PathBuf,
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
