//! Researcher daemon — read-only batch worker. No keys, no streams.
//! Pulls jobs from the mesh, produces artefacts, exits when idle.

mod jobs;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use std::path::PathBuf;
use tracing::info;
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};

#[derive(Parser, Debug)]
struct Args {
    #[arg(long, env = "ZX_FLEET_ID")]    fleet_id: String,
    #[arg(long, env = "ZX_FLEET_TOKEN")] fleet_token: String,
    #[arg(long, env = "ZX_ARTEFACTS", default_value = "./researcher-artefacts")]
                                          artefacts: PathBuf,
    #[arg(long, env = "ZX_HEALTH_BIND", default_value = "127.0.0.1:9304")]
                                          health_bind: String,
    #[arg(long, env = "ZX_WORKERS", default_value_t = num_cpus())]
                                          workers: usize,
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

struct Researcher { args: Args }

#[async_trait]
impl Daemon for Researcher {
    fn name(&self) -> &'static str { "researcher" }
    fn signs_transactions(&self) -> bool { false }

    async fn run(self: Box<Self>) -> Result<()> {
        std::fs::create_dir_all(&self.args.artefacts)?;
        info!(fleet = %self.args.fleet_id, workers = self.args.workers, "researcher starting");
        let health = zerox1_defi_runtime::health::router(self.name());
        let listener = tokio::net::TcpListener::bind(&self.args.health_bind).await?;
        let server = tokio::spawn(async move { axum::serve(listener, health).await });
        let runner = tokio::spawn(jobs::run());
        tokio::select! { r = server => r??, r = runner => r??, }
        Ok(())
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let workers = args.workers;
    let rt = build_runtime(RuntimeProfile::Batch { workers })?;
    rt.block_on(Box::new(Researcher { args }).run())
}
