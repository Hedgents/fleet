//! HedgedJLP daemon — long JLP, short SOL on Adrena. Two legs, one deadline.

mod legs;
mod ledger;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]      fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")]   fleet_token: String,
    #[arg(long, env = "ZX_WALLET")]        wallet: PathBuf,
    #[arg(long, env = "ZX_LEDGER", default_value = "hedgedjlp-ledger.log")]
                                            ledger: PathBuf,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9303")]
                                            health_bind: String,
}

struct HedgedJlp {
    args: Args,
    wallet: Wallet,
    whitelist: SigningWhitelist,
    ledger: ledger::Ledger,
}

#[async_trait]
impl Daemon for HedgedJlp {
    fn name(&self) -> &'static str { "hedgedjlp" }
    fn signs_transactions(&self) -> bool { true }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(fleet = %self.args.fleet_id, "hedgedjlp starting");
        self.ledger.recover_orphans().await?;
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
    let whitelist = SigningWhitelist::new(legs::program_ids());
    let ledger = ledger::Ledger::open(&args.ledger)?;
    let rt = build_runtime(RuntimeProfile::MultiThread { workers: 2 })?;
    rt.block_on(Box::new(HedgedJlp { args, wallet, whitelist, ledger }).run())
}
