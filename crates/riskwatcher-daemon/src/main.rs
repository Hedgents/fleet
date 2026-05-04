//! Risk Watcher daemon — read-only oracle and health monitor.
//! Mandate: emit alerts; never trade. The wallet crate is intentionally
//! not in the dependency graph.

mod alerts;
mod streams;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use tracing::info;
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]
    fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")]
    fleet_token: String,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9301")]
    health_bind: String,
}

struct RiskWatcher {
    args: Args,
}

#[async_trait]
impl Daemon for RiskWatcher {
    fn name(&self) -> &'static str { "riskwatcher" }
    fn signs_transactions(&self) -> bool { false }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(fleet = %self.args.fleet_id, "riskwatcher starting");
        let health = zerox1_defi_runtime::health::router(self.name());
        let listener = tokio::net::TcpListener::bind(&self.args.health_bind).await?;
        let server = tokio::spawn(async move { axum::serve(listener, health).await });
        let streams = tokio::spawn(streams::run());
        tokio::select! {
            r = server => r??,
            r = streams => r??,
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let rt = build_runtime(RuntimeProfile::MultiThread { workers: 4 })?;
    rt.block_on(Box::new(RiskWatcher { args }).run())
}
