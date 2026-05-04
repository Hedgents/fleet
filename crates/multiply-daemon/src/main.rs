//! Multiply daemon — Kamino leveraged LST. Single-flight, sqlite-journaled.
//!
//! TODO(strategy plan): wire the lifted handlers in `kamino.rs` (uses an `AppState
//! { rpc, wallet }`) into a router and select! on it alongside the health server.

mod journal;
mod kamino;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]            fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")]         fleet_token: String,
    #[arg(long, env = "ZX_WALLET")]              wallet: PathBuf,
    #[arg(long, env = "ZX_JOURNAL", default_value = "multiply-journal.sqlite")]
                                                  journal: PathBuf,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9302")]
                                                  health_bind: String,
}

struct Multiply {
    args: Args,
    wallet: Wallet,
    whitelist: SigningWhitelist,
    journal: journal::Journal,
}

#[async_trait]
impl Daemon for Multiply {
    fn name(&self) -> &'static str { "multiply" }
    fn signs_transactions(&self) -> bool { true }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(fleet = %self.args.fleet_id, "multiply starting");
        self.journal.replay().await?;
        let health = zerox1_defi_runtime::health::router(self.name());
        let listener = tokio::net::TcpListener::bind(&self.args.health_bind).await?;
        axum::serve(listener, health).await?;
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let wallet = Wallet::load(&args.wallet)?;
    let whitelist = SigningWhitelist::new(kamino::program_ids());
    let journal = journal::Journal::open(&args.journal)?;
    let rt = build_runtime(RuntimeProfile::SingleThread)?;
    rt.block_on(Box::new(Multiply { args, wallet, whitelist, journal }).run())
}
