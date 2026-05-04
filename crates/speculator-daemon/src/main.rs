//! Speculator daemon — directional execution. Latency-tail-sensitive.
//! Pinned to a single core; current-thread Tokio.

mod exec;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use std::path::PathBuf;
use tracing::{info, warn};
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]    fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")] fleet_token: String,
    #[arg(long, env = "ZX_WALLET")]      wallet: PathBuf,
    #[arg(long, env = "ZX_PIN_CORE")]    pin_core: Option<usize>,
    #[arg(long, env = "ZX_QUOTE_TTL_MS", default_value_t = 750)]
                                          quote_ttl_ms: u64,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9305")]
                                          health_bind: String,
}

struct Speculator {
    args: Args,
    wallet: Wallet,
    whitelist: SigningWhitelist,
}

#[async_trait]
impl Daemon for Speculator {
    fn name(&self) -> &'static str { "speculator" }
    fn signs_transactions(&self) -> bool { true }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(fleet = %self.args.fleet_id, ttl = self.args.quote_ttl_ms, "speculator starting");
        let health = zerox1_defi_runtime::health::router(self.name());
        let listener = tokio::net::TcpListener::bind(&self.args.health_bind).await?;
        axum::serve(listener, health).await?;
        Ok(())
    }
}

fn pin_to_core(core: usize) {
    // Best-effort. On Linux, use sched_setaffinity. On macOS this is a no-op
    // and we just log. The plan does not introduce a third dep just for pinning;
    // if you want hard pinning, add `core_affinity = "0.8"` later.
    warn!(core, "core pinning requested but not implemented in this scaffold");
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    if let Some(c) = args.pin_core { pin_to_core(c); }
    let wallet = Wallet::load(&args.wallet)?;
    let whitelist = SigningWhitelist::new(exec::program_ids());
    let rt = build_runtime(RuntimeProfile::SingleThread)?;
    rt.block_on(Box::new(Speculator { args, wallet, whitelist }).run())
}
