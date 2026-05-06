//! Multiply daemon — Kamino leveraged LST. Single-flight, sqlite-journaled.
//!
//! This binary embeds a full `zerox1-node-enterprise` `NodeService`
//! instance and joins the 0x01 mesh as a long-lived role identity. Unlike
//! riskwatcher, multiply is a *signing* daemon: it keeps the existing
//! `Wallet::load` / `SigningWhitelist` / sqlite journal plumbing and
//! augments it with the embedded mesh node.
//!
//! TODO(strategy plan): wire the lifted handlers in `kamino.rs` (uses
//! an `AppState { rpc, wallet }`) into a per-envelope dispatcher driven
//! from `handle_inbox`, replacing the current axum-router scaffolding.

mod caps;
mod dispatch;
mod journal;
mod kamino;
mod leverage;

use std::path::{Path, PathBuf};
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
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

#[derive(Parser, Debug)]
struct Args {
    /// Stable fleet identifier (logged for cross-cutting introspection).
    #[arg(long, env = "ZX_FLEET_ID", default_value = "01fi-dev")]
    fleet_id: String,

    /// Path to the Solana keypair used to sign Kamino ixns.
    #[arg(long, env = "ZX_WALLET")]
    wallet: PathBuf,

    /// Sqlite single-flight journal path.
    #[arg(long, env = "ZX_JOURNAL", default_value = "multiply-journal.sqlite")]
    journal: PathBuf,

    /// Directory holding role secret files. The daemon reads
    /// `multiply-role.key` (32 raw bytes) from this directory.
    #[arg(long, env = "ZX_SECRETS_DIR", default_value = "/etc/01fi/secrets")]
    secrets_dir: PathBuf,

    /// libp2p listen multiaddr for the embedded node. Default port matches
    /// the old health bind (9302), now multiaddr-shaped.
    #[arg(long, env = "ZX_LISTEN", default_value = "/ip4/0.0.0.0/tcp/9302")]
    listen: String,

    /// Bootstrap peer multiaddrs (repeatable). Empty = no peers; the daemon
    /// still listens but only sees beacons from peers that dial it.
    #[arg(long, env = "ZX_BOOTSTRAP")]
    bootstrap: Vec<String>,

    /// Beacon emit interval, seconds.
    #[arg(long, env = "ZX_BEACON_INTERVAL_SECS", default_value_t = 30)]
    beacon_interval_secs: u64,

    /// Solana RPC URL. Required. Devnet: https://api.devnet.solana.com,
    /// Mainnet: <your-helius-or-triton-url>.
    #[arg(long, env = "ZX_RPC_URL")]
    rpc_url: String,

    /// Maximum collateral the daemon will operate (USDC lamports — 6 decimals).
    /// Defaults to a small bound ($100); raise for real positions but never above
    /// caps::MAX_POSITION_USDC_LAMPORTS ($5M).
    #[arg(long, env = "ZX_MAX_POSITION_USDC", default_value_t = 100_000_000)]
    max_position_usdc_lamports: u64,

    /// Refuse to actually submit transactions; simulate only. Defaults TRUE.
    /// Pass --no-simulate-only to submit for real.
    #[arg(long, env = "ZX_SIMULATE_ONLY", default_value_t = true,
           action = clap::ArgAction::Set)]
    simulate_only: bool,

    /// Require manual Approve envelope before each submission. Defaults TRUE
    /// on mainnet, FALSE on devnet. See --network.
    #[arg(long, env = "ZX_REQUIRE_APPROVAL")]
    require_approval: Option<bool>,

    /// Network: "devnet" or "mainnet". Mainnet additionally requires
    /// --i-understand-this-is-mainnet.
    #[arg(long, env = "ZX_NETWORK", default_value = "devnet")]
    network: String,

    /// Required redundant acknowledgment when --network mainnet. No default.
    #[arg(long)]
    i_understand_this_is_mainnet: bool,
}

struct Multiply {
    args: Args,
    role_identity: RoleIdentity,
    wallet: Arc<Wallet>,
    whitelist: Arc<SigningWhitelist>,
    journal: journal::Journal,
    require_approval: bool,
    rpc: Arc<RpcContext>,
    outbound_nonce: Arc<std::sync::atomic::AtomicU64>,
}

#[async_trait]
impl Daemon for Multiply {
    fn name(&self) -> &'static str { "multiply" }
    fn signs_transactions(&self) -> bool { true }

    async fn run(self: Box<Self>) -> Result<()> {
        info!(
            fleet = %self.args.fleet_id,
            role = %self.role_identity.role().as_str(),
            "multiply starting",
        );

        // Best-effort journal replay at boot — orphans are logged.
        self.journal.replay().await?;

        // Build the embedded node from synthetic argv (avoids consuming the
        // daemon's own argv with NodeConfig::parse()).
        let node_config = build_node_config(&self.args, &self.role_identity)?;
        let service = NodeService::build(node_config).await?;
        let handle = service.handle();

        // Shared outbound nonce counter for all envelope types (BEACONs, Reports, etc.)
        let outbound_nonce = Arc::new(std::sync::atomic::AtomicU64::new(1));

        let beacon_interval = Duration::from_secs(self.args.beacon_interval_secs);
        let beacon_handle = handle.clone();
        let beacon_role = self.role_identity.clone();
        let beacon_nonce = outbound_nonce.clone();
        let dispatch_handle = handle.clone();
        let dispatch_ctx = dispatch::DispatchCtx {
            rpc: self.rpc.clone(),
            wallet: self.wallet.clone(),
            whitelist: self.whitelist.clone(),
            role_identity: self.role_identity.clone(),
            simulate_only: self.args.simulate_only,
            require_approval: self.require_approval,
            nonce: outbound_nonce.clone(),
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
            r = dispatch::run(dispatch_handle, dispatch_ctx) => {
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
/// to `<secrets_dir>/.runtime-keypair-multiply` (raw 32 bytes — matches
/// `AgentIdentity::load_or_generate`'s expected format).
fn build_node_config(args: &Args, role_id: &RoleIdentity) -> Result<NodeConfig> {
    let keypair_path = args.secrets_dir.join(".runtime-keypair-multiply");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing keypair to {}", keypair_path.display()))?;

    let mut argv: Vec<String> = vec!["multiply".to_string()];
    argv.push("--listen-addr".into());
    argv.push(args.listen.clone());
    argv.push("--keypair-path".into());
    argv.push(keypair_path.display().to_string());
    argv.push("--agent-name".into());
    argv.push(format!("multiply-{}", args.fleet_id));
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
    nonce: Arc<std::sync::atomic::AtomicU64>,
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
        let n = nonce.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

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

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Network sanity gates.
    if args.network != "devnet" && args.network != "mainnet" {
        anyhow::bail!("--network must be 'devnet' or 'mainnet', got {:?}", args.network);
    }
    if args.network == "mainnet" && !args.i_understand_this_is_mainnet {
        anyhow::bail!(
            "--network mainnet requires --i-understand-this-is-mainnet \
             (this exists to make mainnet promotion explicit)"
        );
    }

    // Cap enforcement on max_position_usdc_lamports.
    if args.max_position_usdc_lamports > caps::MAX_POSITION_USDC_LAMPORTS {
        anyhow::bail!(
            "--max-position-usdc-lamports {} exceeds hard cap {}",
            args.max_position_usdc_lamports,
            caps::MAX_POSITION_USDC_LAMPORTS
        );
    }

    // Resolve require_approval default: true on mainnet, false on devnet.
    let require_approval = args.require_approval.unwrap_or(args.network == "mainnet");

    info!(
        network = %args.network,
        rpc_url = %args.rpc_url,
        simulate_only = args.simulate_only,
        require_approval,
        max_position_usdc_lamports = args.max_position_usdc_lamports,
        "multiply args validated",
    );

    // Existing multiply boot logic — Wallet/whitelist/journal are kept and
    // augmented with the embedded mesh node, not replaced.
    let wallet = Arc::new(Wallet::load(&args.wallet)?);
    let whitelist = Arc::new(SigningWhitelist::new(kamino::program_ids()));
    let journal = journal::Journal::open(&args.journal)?;
    let rpc = Arc::new(RpcContext::new(
        args.rpc_url.clone(),
        CommitmentConfig::confirmed(),
    ));

    let rt = build_runtime(RuntimeProfile::SingleThread)?;
    rt.block_on(async move {
        let secrets = FileSource::new(&args.secrets_dir);
        let role_identity =
            load_role_identity(&secrets, Role::Multiply, "multiply-role.key")
                .await
                .context("loading multiply role key")?;

        let outbound_nonce = Arc::new(std::sync::atomic::AtomicU64::new(1));

        Box::new(Multiply {
            args,
            role_identity,
            wallet,
            whitelist,
            journal,
            require_approval,
            rpc,
            outbound_nonce,
        })
        .run()
        .await
    })
}
