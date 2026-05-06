//! Researcher daemon — read-only batch worker. No keys, no streams.
//! Pulls jobs from the mesh, produces artefacts, exits when idle.
//!
//! This binary embeds a full `zerox1-node-enterprise` `NodeService`
//! instance and joins the 0x01 mesh as a long-lived role identity. It
//! does not run an HTTP server — every interaction is via signed
//! envelopes on the mesh.
//!
//! Mandate: read-only. The wallet crate is intentionally not in the
//! dependency graph (authority isolation invariant).

mod jobs;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Parser;
use solana_sdk::commitment_config::CommitmentConfig;
use tracing::{info, warn};

use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_runtime::identity::{Role, RoleIdentity};
use zerox1_defi_runtime::rpc::RpcContext;
use zerox1_defi_runtime::secrets::{FileSource, load_role_identity};

use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

use researcher_daemon::dedup::EmissionTracker;
use researcher_daemon::watchers;

#[derive(Parser, Debug)]
struct Args {
    /// Stable fleet identifier (logged for cross-cutting introspection).
    #[arg(long, env = "ZX_FLEET_ID", default_value = "01fi-dev")]
    fleet_id: String,

    /// Directory holding role secret files. The daemon reads
    /// `researcher-role.key` (32 raw bytes) from this directory.
    #[arg(long, env = "ZX_SECRETS_DIR", default_value = "/etc/01fi/secrets")]
    secrets_dir: PathBuf,

    /// Output directory for batch artefacts.
    #[arg(long, env = "ZX_ARTEFACTS", default_value = "./researcher-artefacts")]
    artefacts: PathBuf,

    /// Number of batch workers (defaults to host parallelism).
    #[arg(long, env = "ZX_WORKERS", default_value_t = num_cpus())]
    workers: usize,

    /// libp2p listen multiaddr for the embedded node.
    #[arg(long, env = "ZX_LISTEN", default_value = "/ip4/0.0.0.0/tcp/9304")]
    listen: String,

    /// Bootstrap peer multiaddrs (repeatable). Empty = no peers; the daemon
    /// still listens but only sees beacons from peers that dial it.
    #[arg(long, env = "ZX_BOOTSTRAP")]
    bootstrap: Vec<String>,

    /// Beacon emit interval, seconds.
    #[arg(long, env = "ZX_BEACON_INTERVAL_SECS", default_value_t = 30)]
    beacon_interval_secs: u64,

    /// Solana RPC URL — used by chain-reading watchers (lending_rate
    /// watcher polls Kamino reserves here).
    #[arg(long, env = "ZX_RPC_URL", default_value = "https://api.devnet.solana.com")]
    rpc_url: String,

    /// Lending watcher tick interval, seconds.
    #[arg(long, default_value_t = 60)]
    lending_poll_interval_secs: u64,

    /// Reserves to watch. Format: `name:base58_pubkey:asset_enum`. Repeat
    /// for multiple. Example: `usdc:DGQRoyx...:USDC`. Empty = lending
    /// watcher disabled.
    #[arg(long)]
    lending_reserve: Vec<String>,

    /// Perp funding watcher tick interval, seconds.
    #[arg(long, default_value_t = 60)]
    funding_poll_interval_secs: u64,

    /// Drift perp markets to watch. Format: `name:base58_pubkey:asset_enum`.
    /// Example: `sol-perp:8UJgxaiQx5nTrdDgph5FiahMmzduuLTLf5WmsPegYA6W:SOL`.
    /// Empty = funding watcher disabled.
    #[arg(long)]
    funding_market: Vec<String>,

    /// Initial subscriber list — recipients of MarketSignal envelopes.
    /// Hex-encoded role pubkeys (32 bytes = 64 hex chars). Repeat for
    /// multiple. v0: must be passed explicitly. Future: auto-discover via
    /// BEACON.
    #[arg(long)]
    subscriber: Vec<String>,
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
}

struct Researcher {
    args: Args,
    role_identity: RoleIdentity,
}

#[async_trait]
impl Daemon for Researcher {
    fn name(&self) -> &'static str { "researcher" }
    fn signs_transactions(&self) -> bool { false }

    async fn run(self: Box<Self>) -> Result<()> {
        std::fs::create_dir_all(&self.args.artefacts)
            .with_context(|| format!("creating artefacts dir {}", self.args.artefacts.display()))?;
        info!(
            fleet = %self.args.fleet_id,
            workers = self.args.workers,
            artefacts = %self.args.artefacts.display(),
            "researcher starting",
        );

        // Build the embedded node from synthetic argv, write the role seed
        // to a keypair file the node loop can mmap as identity.
        let node_config = build_node_config(&self.args, &self.role_identity)?;
        let service = NodeService::build(node_config).await?;
        let handle = service.handle();

        // Shared outbound nonce for ALL outbound envelopes (BEACONs +
        // MarketSignals). Watchers monotonically increment this so the
        // mesh sees a single consistent stream from the role identity.
        let outbound_nonce = Arc::new(AtomicU64::new(1));

        // Shared dedup tracker — future watchers (M4+) plug in here.
        let dedup = Arc::new(EmissionTracker::default());

        // Parse lending reserve specs + subscriber pubkeys from CLI.
        let reserves = parse_reserves(&self.args.lending_reserve)?;
        let perp_markets = parse_perp_markets(&self.args.funding_market)?;
        let subscribers_vec = parse_subscribers(&self.args.subscriber)?;
        let subscribers = Arc::new(tokio::sync::RwLock::new(subscribers_vec));

        // RpcContext for chain-reading watchers.
        let rpc = Arc::new(RpcContext::new(
            self.args.rpc_url.clone(),
            CommitmentConfig::confirmed(),
        ));

        let beacon_interval = Duration::from_secs(self.args.beacon_interval_secs);
        let beacon_handle = handle.clone();
        let beacon_role = self.role_identity.clone();
        let beacon_nonce = outbound_nonce.clone();

        let inbox_handle = handle.clone();

        // Watcher: lending rate. Disabled when no reserves passed.
        let lending_fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> =
            if reserves.is_empty() {
                info!("lending_rate watcher disabled (no --lending-reserve)");
                Box::pin(std::future::pending())
            } else {
                let lending_rpc = rpc.clone();
                let lending_handle = handle.clone();
                let lending_role = self.role_identity.clone();
                let lending_nonce = outbound_nonce.clone();
                let lending_dedup = dedup.clone();
                let lending_subs = subscribers.clone();
                let lending_interval =
                    Duration::from_secs(self.args.lending_poll_interval_secs);
                Box::pin(async move {
                    watchers::lending_rate::run(
                        lending_rpc,
                        lending_handle,
                        lending_role,
                        lending_nonce,
                        lending_dedup,
                        reserves,
                        lending_subs,
                        lending_interval,
                    )
                    .await
                })
            };

        // Watcher: perp funding. Disabled when no markets passed.
        let funding_fut: std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>> =
            if perp_markets.is_empty() {
                info!("perp_funding watcher disabled (no --funding-market)");
                Box::pin(std::future::pending())
            } else {
                let funding_rpc = rpc.clone();
                let funding_handle = handle.clone();
                let funding_role = self.role_identity.clone();
                let funding_nonce = outbound_nonce.clone();
                let funding_dedup = dedup.clone();
                let funding_subs = subscribers.clone();
                let funding_interval =
                    Duration::from_secs(self.args.funding_poll_interval_secs);
                Box::pin(async move {
                    watchers::perp_funding::run(
                        funding_rpc,
                        funding_handle,
                        funding_role,
                        funding_nonce,
                        funding_dedup,
                        perp_markets,
                        funding_subs,
                        funding_interval,
                    )
                    .await
                })
            };

        tokio::select! {
            r = service.run() => {
                warn!(?r, "node loop exited");
                r
            }
            r = emit_beacons(beacon_handle, beacon_role, beacon_interval, beacon_nonce) => {
                warn!(?r, "beacon emitter exited");
                r
            }
            r = handle_inbox(inbox_handle) => {
                warn!(?r, "inbox dispatcher exited");
                r
            }
            r = jobs::run() => {
                warn!(?r, "jobs loop exited");
                r
            }
            r = lending_fut => {
                warn!(?r, "lending watcher exited");
                r
            }
            r = funding_fut => {
                warn!(?r, "perp_funding watcher exited");
                r
            }
        }
    }
}

/// Parse `--lending-reserve` strings into `ReserveSpec` values.
fn parse_reserves(specs: &[String]) -> Result<Vec<watchers::lending_rate::ReserveSpec>> {
    specs
        .iter()
        .map(|s| watchers::lending_rate::parse_reserve_spec(s))
        .collect()
}

/// Parse `--funding-market` strings into `PerpMarketSpec` values.
fn parse_perp_markets(specs: &[String]) -> Result<Vec<watchers::perp_funding::PerpMarketSpec>> {
    specs
        .iter()
        .map(|s| watchers::perp_funding::parse_market_spec(s))
        .collect()
}

/// Parse `--subscriber` hex strings into 32-byte pubkeys.
fn parse_subscribers(items: &[String]) -> Result<Vec<[u8; 32]>> {
    let mut out = Vec::with_capacity(items.len());
    for s in items {
        let bytes = hex::decode(s)
            .with_context(|| format!("subscriber {s:?} is not valid hex"))?;
        if bytes.len() != 32 {
            anyhow::bail!(
                "subscriber {s:?} decodes to {} bytes; expected 32",
                bytes.len()
            );
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        out.push(arr);
    }
    Ok(out)
}

/// Translate the daemon's `Args` + role seed into a `NodeConfig`.
///
/// We use `NodeConfig::try_parse_from(synthetic_argv)` so we get the same
/// defaulting behavior as the standalone `zerox1-node-enterprise` binary,
/// without consuming the daemon's own CLI args. The role seed is written
/// to `<secrets_dir>/.runtime-keypair-researcher` (raw 32 bytes — matches
/// `AgentIdentity::load_or_generate`'s expected format).
fn build_node_config(args: &Args, role_id: &RoleIdentity) -> Result<NodeConfig> {
    let keypair_path = args.secrets_dir.join(".runtime-keypair-researcher");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing keypair to {}", keypair_path.display()))?;

    let mut argv: Vec<String> = vec!["researcher".to_string()];
    argv.push("--listen-addr".into());
    argv.push(args.listen.clone());
    argv.push("--keypair-path".into());
    argv.push(keypair_path.display().to_string());
    argv.push("--agent-name".into());
    argv.push(format!("researcher-{}", args.fleet_id));
    for boot in &args.bootstrap {
        argv.push("--bootstrap".into());
        argv.push(boot.clone());
    }

    NodeConfig::try_parse_from(&argv)
        .map_err(|e| anyhow::anyhow!("synthesizing NodeConfig: {e}"))
}

/// Write a 32-byte Ed25519 seed to `path` in the raw format expected by
/// `AgentIdentity::load_or_generate` (which calls `std::fs::read` and
/// expects exactly 32 bytes).
fn write_keypair(path: &Path, seed: &[u8; 32]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(seed)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, seed)?;
    }
    Ok(())
}

/// Emit a Beacon envelope onto the mesh every `interval`.
///
/// Beacon payload convention: `[agent_id(32)][verifying_key(32)][name(utf-8)]`.
/// For now `agent_id == verifying_key` (no on-chain registration in the
/// enterprise mesh — see `node-enterprise/src/identity.rs`).
async fn emit_beacons(
    handle: NodeHandle,
    role_id: RoleIdentity,
    interval: Duration,
    nonce: Arc<AtomicU64>,
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(role_id.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();

    loop {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let payload = build_beacon_payload(&role_id, &signing_key);
        let nonce_v = nonce.fetch_add(1, Ordering::Relaxed);

        let env = Envelope::build(
            MsgType::Beacon,
            sender,
            BROADCAST_RECIPIENT,
            now_secs,
            nonce_v,
            [0u8; 16],
            payload,
            &signing_key,
        );

        match handle.send(env).await {
            Ok(()) => info!(role = %role_id.role().as_str(), nonce = nonce_v, "beacon emitted"),
            Err(e) => warn!(?e, "beacon send failed"),
        }

        tokio::time::sleep(interval).await;
    }
}

fn build_beacon_payload(
    role_id: &RoleIdentity,
    signing_key: &ed25519_dalek::SigningKey,
) -> Vec<u8> {
    let vk = signing_key.verifying_key().to_bytes();
    let name = role_id.role().as_str().as_bytes();
    let mut buf = Vec::with_capacity(32 + 32 + name.len());
    buf.extend_from_slice(&vk);          // agent_id (= verifying_key in enterprise mode)
    buf.extend_from_slice(&vk);          // verifying_key
    buf.extend_from_slice(name);         // display name
    buf
}

/// Drain the inbound envelope stream, logging each delivery. The future
/// strategy plan replaces this with per-MsgType dispatch (e.g. ingest
/// FleetResearchRequest, fan out artefact-ready notifications).
async fn handle_inbox(mut handle: NodeHandle) -> Result<()> {
    while let Some(env) = handle.recv().await {
        info!(
            msg_type = ?env.msg_type,
            sender = %hex::encode(env.sender),
            nonce = env.nonce,
            "inbox envelope",
        );
    }
    warn!("inbox channel closed; daemon exiting");
    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let workers = args.workers;

    // Load the role identity before constructing the runtime so we can fail
    // fast on missing secrets without paying the cost of spinning up a
    // multi-thread tokio runtime.
    let rt = build_runtime(RuntimeProfile::Batch { workers })?;
    rt.block_on(async move {
        let secrets = FileSource::new(&args.secrets_dir);
        let role_identity = load_role_identity(&secrets, Role::Researcher, "researcher-role.key")
            .await
            .context("loading researcher role key")?;
        Box::new(Researcher { args, role_identity }).run().await
    })
}
