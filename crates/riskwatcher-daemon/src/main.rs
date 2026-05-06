//! Risk Watcher daemon — read-only oracle and health monitor.
//! Mandate: emit alerts; never trade. The wallet crate is intentionally
//! not in the dependency graph.
//!
//! This binary embeds a full `zerox1-node-enterprise` `NodeService`
//! instance and joins the 0x01 mesh as a long-lived role identity. It
//! does not run an HTTP server — every interaction is via signed
//! envelopes on the mesh.
//!
//! TODO(strategy plan): wire the lifted Pyth handler in `alerts.rs` (uses an
//! `AppState { rpc, pyth_cache }` — no wallet field) into a per-envelope
//! handler dispatched from `handle_inbox`.

mod alerts;
mod streams;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::{Parser, ValueEnum};
use tracing::{info, warn};

use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_runtime::identity::{Role, RoleIdentity};
use zerox1_defi_runtime::secrets::{FileSource, load_role_identity};

use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

#[derive(Parser, Debug)]
struct Args {
    /// Stable fleet identifier (logged for cross-cutting introspection).
    #[arg(long, env = "ZX_FLEET_ID", default_value = "01fi-dev")]
    fleet_id: String,

    /// Directory holding role secret files. The daemon reads
    /// `riskwatcher-role.key` (32 raw bytes) from this directory.
    #[arg(long, env = "ZX_SECRETS_DIR", default_value = "/etc/01fi/secrets")]
    secrets_dir: PathBuf,

    /// libp2p listen multiaddr for the embedded node.
    #[arg(long, env = "ZX_LISTEN", default_value = "/ip4/0.0.0.0/tcp/9300")]
    listen: String,

    /// Bootstrap peer multiaddrs (repeatable). Empty = no peers; the daemon
    /// still listens but only sees beacons from peers that dial it.
    #[arg(long, env = "ZX_BOOTSTRAP")]
    bootstrap: Vec<String>,

    /// Beacon emit interval, seconds.
    #[arg(long, env = "ZX_BEACON_INTERVAL_SECS", default_value_t = 30)]
    beacon_interval_secs: u64,

    /// Solana JSON-RPC endpoint. Consumed in M4 (Kamino obligation poller)
    /// and M6 (escalate emission). Wired now so later milestones don't
    /// churn the CLI surface again.
    #[arg(long, env = "ZX_RPC_URL", default_value = "http://localhost:8899")]
    rpc_url: String,

    /// Solana network the daemon is observing. Consumed in M4 poller and
    /// M6 escalate (selects program IDs / market addresses). Devnet by
    /// default — mainnet must be opted into explicitly.
    #[arg(long, env = "ZX_NETWORK", value_enum, default_value_t = Network::Devnet)]
    network: Network,
}

#[derive(ValueEnum, Clone, Debug)]
enum Network {
    Devnet,
    Mainnet,
}

struct RiskWatcher {
    args: Args,
}

#[async_trait]
impl Daemon for RiskWatcher {
    fn name(&self) -> &'static str { "riskwatcher" }
    fn signs_transactions(&self) -> bool { false }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(
            fleet = %self.args.fleet_id,
            rpc_url = %self.args.rpc_url,
            network = ?self.args.network,
            "riskwatcher starting",
        );

        // Load role identity from the secrets backend. File-based for now;
        // production would swap in a Vault-backed SecretSource here.
        let secrets = FileSource::new(&self.args.secrets_dir);
        let role_id = load_role_identity(&secrets, Role::RiskWatcher, "riskwatcher-role.key")
            .await
            .context("loading riskwatcher role key")?;

        // Build the embedded node from synthetic argv, write the role seed
        // to a keypair file the node loop can mmap as identity.
        let node_config = build_node_config(&self.args, &role_id)?;
        let service = NodeService::build(node_config).await?;
        let handle = service.handle();

        let beacon_interval = Duration::from_secs(self.args.beacon_interval_secs);
        let beacon_handle = handle.clone();
        let beacon_role = role_id.clone();

        let inbox_handle = handle.clone();

        // Shared outbound nonce counter. Today only `emit_beacons` claims
        // from it, but M6 (escalate emitter) will share it too — wiring
        // the Arc<AtomicU64> now mirrors the multiply-daemon pattern and
        // avoids churn when escalate lands.
        let outbound_nonce = Arc::new(AtomicU64::new(1));
        let beacon_nonce = outbound_nonce.clone();

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
            r = streams::run() => {
                warn!(?r, "streams loop exited");
                r
            }
        }
    }
}

/// Translate the daemon's `Args` + role seed into a `NodeConfig`.
///
/// We use `NodeConfig::try_parse_from(synthetic_argv)` so we get the same
/// defaulting behavior as the standalone `zerox1-node-enterprise` binary,
/// without consuming the daemon's own CLI args. The role seed is written
/// to `<secrets_dir>/.runtime-keypair-riskwatcher` (raw 32 bytes — matches
/// `AgentIdentity::load_or_generate`'s expected format).
fn build_node_config(args: &Args, role_id: &RoleIdentity) -> Result<NodeConfig> {
    let keypair_path = args.secrets_dir.join(".runtime-keypair-riskwatcher");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing keypair to {}", keypair_path.display()))?;

    let mut argv: Vec<String> = vec!["riskwatcher".to_string()];
    argv.push("--listen-addr".into());
    argv.push(args.listen.clone());
    argv.push("--keypair-path".into());
    argv.push(keypair_path.display().to_string());
    argv.push("--agent-name".into());
    argv.push(format!("riskwatcher-{}", args.fleet_id));
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

        // Claim the next nonce from the shared counter.
        let n = nonce.fetch_add(1, Ordering::Relaxed);

        let env = Envelope::build(
            MsgType::Beacon,
            sender,
            BROADCAST_RECIPIENT,
            now_secs,
            n,
            [0u8; 16],
            payload,
            &signing_key,
        );

        match handle.send(env).await {
            Ok(()) => info!(role = %role_id.role().as_str(), nonce = n, "beacon emitted"),
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
/// FleetPriceTick from the orchestrator, fan out RiskAlert).
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
    let rt = build_runtime(RuntimeProfile::MultiThread { workers: 4 })?;
    rt.block_on(Box::new(RiskWatcher { args }).run())
}
