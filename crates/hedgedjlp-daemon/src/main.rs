//! HedgedJLP daemon — long JLP, short SOL on Adrena. Two legs, one deadline.
//!
//! This binary embeds a full `zerox1-node-enterprise` `NodeService`
//! instance and joins the 0x01 mesh as a long-lived role identity. Like
//! multiply, hedgedjlp is a *signing* daemon: it keeps the existing
//! `Wallet::load` / `SigningWhitelist` / JSONL ledger plumbing and
//! augments it with the embedded mesh node. The runtime profile is
//! `MultiThread { workers: 2 }` — one Tokio worker per leg of the hedge.
//!
//! TODO(strategy plan): wire the lifted handlers in `legs.rs` into a
//! per-envelope dispatcher driven from `handle_inbox`.

mod legs;
mod ledger;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::Parser;
use tracing::{info, warn};

use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_runtime::identity::{Role, RoleIdentity};
use zerox1_defi_runtime::secrets::{FileSource, load_role_identity};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

#[derive(Parser, Debug)]
struct Args {
    /// Stable fleet identifier (logged for cross-cutting introspection).
    #[arg(long, env = "ZX_FLEET_ID", default_value = "01fi-dev")]
    fleet_id: String,

    /// Path to the Solana keypair used to sign hedge-leg ixns.
    #[arg(long, env = "ZX_WALLET")]
    wallet: PathBuf,

    /// JSONL leg-pair ledger path.
    #[arg(long, env = "ZX_LEDGER", default_value = "hedgedjlp-ledger.log")]
    ledger: PathBuf,

    /// Directory holding role secret files. The daemon reads
    /// `hedgedjlp-role.key` (32 raw bytes) from this directory.
    #[arg(long, env = "ZX_SECRETS_DIR", default_value = "/etc/01fi/secrets")]
    secrets_dir: PathBuf,

    /// libp2p listen multiaddr for the embedded node. Default port matches
    /// the old health bind (9303), now multiaddr-shaped.
    #[arg(long, env = "ZX_LISTEN", default_value = "/ip4/0.0.0.0/tcp/9303")]
    listen: String,

    /// Bootstrap peer multiaddrs (repeatable). Empty = no peers; the daemon
    /// still listens but only sees beacons from peers that dial it.
    #[arg(long, env = "ZX_BOOTSTRAP")]
    bootstrap: Vec<String>,

    /// Beacon emit interval, seconds.
    #[arg(long, env = "ZX_BEACON_INTERVAL_SECS", default_value_t = 30)]
    beacon_interval_secs: u64,
}

struct HedgedJlp {
    args: Args,
    role_identity: RoleIdentity,
    #[allow(dead_code)] // wired in by the strategy plan
    wallet: Wallet,
    #[allow(dead_code)] // wired in by the strategy plan
    whitelist: SigningWhitelist,
    ledger: ledger::Ledger,
}

#[async_trait]
impl Daemon for HedgedJlp {
    fn name(&self) -> &'static str { "hedgedjlp" }
    fn signs_transactions(&self) -> bool { true }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(
            fleet = %self.args.fleet_id,
            role = %self.role_identity.role().as_str(),
            "hedgedjlp starting",
        );

        // Best-effort ledger replay at boot — orphans are logged.
        self.ledger.recover_orphans().await?;

        // Build the embedded node from synthetic argv (avoids consuming the
        // daemon's own argv with NodeConfig::parse()).
        let node_config = build_node_config(&self.args, &self.role_identity)?;
        let service = NodeService::build(node_config).await?;
        let handle = service.handle();

        let beacon_interval = Duration::from_secs(self.args.beacon_interval_secs);
        let beacon_handle = handle.clone();
        let beacon_role = self.role_identity.clone();
        let inbox_handle = handle.clone();

        tokio::select! {
            r = service.run() => {
                warn!(?r, "node loop exited");
                r
            }
            r = emit_beacons(beacon_handle, beacon_role, beacon_interval) => {
                warn!(?r, "beacon emitter exited");
                r
            }
            r = handle_inbox(inbox_handle) => {
                warn!(?r, "inbox dispatcher exited");
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
/// to `<secrets_dir>/.runtime-keypair-hedgedjlp` (raw 32 bytes — matches
/// `AgentIdentity::load_or_generate`'s expected format).
fn build_node_config(args: &Args, role_id: &RoleIdentity) -> Result<NodeConfig> {
    let keypair_path = args.secrets_dir.join(".runtime-keypair-hedgedjlp");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing keypair to {}", keypair_path.display()))?;

    let mut argv: Vec<String> = vec!["hedgedjlp".to_string()];
    argv.push("--listen-addr".into());
    argv.push(args.listen.clone());
    argv.push("--keypair-path".into());
    argv.push(keypair_path.display().to_string());
    argv.push("--agent-name".into());
    argv.push(format!("hedgedjlp-{}", args.fleet_id));
    for boot in args.bootstrap.iter().filter(|b| !b.is_empty()) {
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
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(role_id.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();
    let mut nonce: u64 = 1;

    loop {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let payload = build_beacon_payload(&role_id, &signing_key);

        let env = Envelope::build(
            MsgType::Beacon,
            sender,
            BROADCAST_RECIPIENT,
            now_secs,
            nonce,
            [0u8; 16],
            payload,
            &signing_key,
        );

        match handle.send(env).await {
            Ok(()) => info!(role = %role_id.role().as_str(), nonce, "beacon emitted"),
            Err(e) => warn!(?e, "beacon send failed"),
        }
        nonce = nonce.wrapping_add(1);

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
/// FleetIntent from the orchestrator, fan out signed leg-pair receipts).
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

    // Existing hedgedjlp boot logic — Wallet/whitelist/ledger are kept and
    // augmented with the embedded mesh node, not replaced.
    let wallet = Wallet::load(&args.wallet)?;
    let whitelist = SigningWhitelist::new(legs::program_ids());
    let ledger = ledger::Ledger::open(&args.ledger)?;

    let rt = build_runtime(RuntimeProfile::MultiThread { workers: 2 })?;
    rt.block_on(async move {
        let secrets = FileSource::new(&args.secrets_dir);
        let role_identity =
            load_role_identity(&secrets, Role::HedgedJlp, "hedgedjlp-role.key")
                .await
                .context("loading hedgedjlp role key")?;

        Box::new(HedgedJlp {
            args,
            role_identity,
            wallet,
            whitelist,
            ledger,
        })
        .run()
        .await
    })
}
