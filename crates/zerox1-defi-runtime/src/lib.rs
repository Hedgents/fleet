#![allow(clippy::should_implement_trait)]
#![allow(clippy::too_many_arguments)]

//! Shared runtime primitives for every 01fi daemon.
//!
//! Each daemon binary picks a `RuntimeProfile`, implements `Daemon`, and
//! calls `run(profile, daemon)` from `main`. The profile drives Tokio
//! flavor, worker count, and whether a health server binds.

pub mod fleet_rates;
pub mod identity;
pub mod pairing;
pub mod persistence;
pub mod replay;
pub mod role_registry;
pub mod rpc;
pub mod secrets;

use anyhow::Result;

/// How a daemon's Tokio runtime is configured.
#[derive(Debug, Clone, Copy)]
pub enum RuntimeProfile {
    /// Single-thread, current-thread runtime. Use for serial executors
    /// (multiply) and other pinned-latency loops.
    SingleThread,
    /// Multi-thread runtime with a fixed worker count. Use for streaming
    /// (riskwatcher: 4) and two-leg execution (hedgedjlp: 2).
    MultiThread { workers: usize },
    /// One-shot: do the work, exit. Use for stablefloor.
    OneShot,
    /// Throughput-bound batch runtime with Rayon-friendly worker count.
    Batch { workers: usize },
}

#[async_trait::async_trait]
pub trait Daemon: Send + Sync + 'static {
    /// Stable name shown in logs and fleet introspection.
    fn name(&self) -> &'static str;

    /// Whether this daemon can produce signed Solana transactions.
    /// Enforced at compile time by whether the binary depends on
    /// `zerox1-defi-wallet` — this method is documentation only.
    fn signs_transactions(&self) -> bool;

    /// Daemon main loop. Returns when shutdown is requested.
    async fn run(self: Box<Self>) -> Result<()>;
}

pub fn build_runtime(profile: RuntimeProfile) -> Result<tokio::runtime::Runtime> {
    let mut builder = match profile {
        RuntimeProfile::SingleThread | RuntimeProfile::OneShot => {
            tokio::runtime::Builder::new_current_thread()
        }
        RuntimeProfile::MultiThread { workers } | RuntimeProfile::Batch { workers } => {
            let mut b = tokio::runtime::Builder::new_multi_thread();
            b.worker_threads(workers);
            b
        }
    };
    builder.enable_all().build().map_err(Into::into)
}
