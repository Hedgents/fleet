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

mod approval;
mod caps;
mod dispatch;
mod journal;
mod kamino;
mod leverage;
mod liq_monitor;
mod pnl;
mod reporter;
mod seed;
// unwind: pure round-builders + strategy decision. The caller-side wire-up
// (dispatch.rs WithdrawMultiply handler) arrives in commit 5; in the
// commit-4 state every function is unused outside its own tests, hence the
// dead_code allow on the module decl.
#[allow(dead_code)]
mod unwind;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use solana_sdk::commitment_config::CommitmentConfig;
use tracing::{debug, info, warn};

use zerox1_defi_runtime::identity::{Role, RoleIdentity};
use zerox1_defi_runtime::rpc::RpcContext;
use zerox1_defi_runtime::secrets::{load_role_identity, FileSource};
use zerox1_defi_runtime::{build_runtime, Daemon, RuntimeProfile};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

#[derive(Parser, Debug)]
#[command(name = "multiply-daemon")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Run the multiply daemon (long-running mesh peer + leverage executor).
    Run(RunArgs),
    /// Read the pnl log and print a trailing-APR readout.
    Report(ReportArgs),
}

#[derive(Parser, Debug)]
struct RunArgs {
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

    /// Recipient agent_id (32-byte hex) for proactive Escalate envelopes from
    /// the liquidation monitor. If unset, the monitor logs at warn/error level
    /// but does not send mesh Escalates. Mainnet operators are strongly
    /// encouraged to set this so the orchestrator gets fast notification on
    /// position drift.
    #[arg(long, env = "ZX_ORCHESTRATOR_AGENT_ID")]
    orchestrator_agent_id: Option<String>,

    /// 32-byte hex pubkey of the trusted riskwatcher daemon. Critical
    /// + LiquidationDistance Escalate envelopes from this pubkey trigger
    /// a 5-minute pause on new AssignMultiply (returned with error_code=4).
    /// Escalates from any other sender are ignored. Optional — when
    /// unset, the soft-veto is disabled and Escalates are observed only.
    #[arg(long, env = "ZX_RISKWATCHER")]
    riskwatcher: Option<String>,

    /// Path to JSONL pnl log. The beacon loop appends one snapshot per tick.
    /// Read with `multiply-daemon report --log <path>`.
    #[arg(long, env = "ZX_PNL_LOG", default_value = "multiply-pnl.jsonl")]
    pnl_log: PathBuf,

    /// Paper-trading notional principal in USDC lamports.
    /// Used to compute simulated P&L at the multiply strategy's live APR.
    /// Does not affect real on-chain positions.
    #[arg(long, default_value_t = 1_000_000_000)]
    paper_principal_usdc_lamports: u64,
}

#[derive(Parser, Debug)]
struct ReportArgs {
    /// Path to the pnl JSONL log written by the daemon's beacon loop.
    #[arg(long, default_value = "multiply-pnl.jsonl")]
    log: PathBuf,
    /// Trailing window for APR computation, seconds.
    #[arg(long, default_value_t = 86400)]
    since_secs: u64,
}

struct Multiply {
    args: RunArgs,
    role_identity: RoleIdentity,
    wallet: Arc<Wallet>,
    whitelist: Arc<SigningWhitelist>,
    journal: journal::Journal,
    require_approval: bool,
    rpc: Arc<RpcContext>,
    outbound_nonce: Arc<std::sync::atomic::AtomicU64>,
    orchestrator_agent_id: Option<[u8; 32]>,
    /// Decoded `--riskwatcher` pubkey. `None` disables the soft-veto.
    riskwatcher_pubkey: Option<[u8; 32]>,
}

#[async_trait]
impl Daemon for Multiply {
    fn name(&self) -> &'static str {
        "multiply"
    }
    fn signs_transactions(&self) -> bool {
        true
    }

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
        let monitor_ctx = liq_monitor::LiqMonitorCtx {
            rpc: self.rpc.clone(),
            user: self.wallet.pubkey(),
            lending_market: zerox1_defi_protocols::constants::KAMINO_MAIN_MARKET,
            role_identity: self.role_identity.clone(),
            orchestrator_agent_id: self.orchestrator_agent_id,
            outbound_nonce: outbound_nonce.clone(),
        };
        let pnl_log_path = self.args.pnl_log.clone();
        let pnl_paper_principal = self.args.paper_principal_usdc_lamports as f64 / 1_000_000.0;
        let pnl_start_ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let dispatch_handle = handle.clone();
        let approval_queue = Arc::new(approval::ApprovalQueue::new());
        let dispatch_ctx = dispatch::DispatchCtx {
            rpc: self.rpc.clone(),
            wallet: self.wallet.clone(),
            whitelist: self.whitelist.clone(),
            role_identity: self.role_identity.clone(),
            simulate_only: self.args.simulate_only,
            require_approval: self.require_approval,
            nonce: outbound_nonce.clone(),
            args_max_position_usdc_lamports: self.args.max_position_usdc_lamports,
            approval_queue,
            orchestrator_agent_id: self.orchestrator_agent_id,
            riskwatcher_pubkey: self.riskwatcher_pubkey,
            paused_until_unix: Arc::new(std::sync::Mutex::new(None)),
        };

        tokio::select! {
            r = service.run() => {
                warn!(?r, "node loop exited");
                r
            }
            r = emit_beacons(beacon_handle, beacon_role, beacon_interval, beacon_nonce, monitor_ctx, pnl_log_path, pnl_start_ts, pnl_paper_principal) => {
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

/// Translate the daemon's `RunArgs` + role seed into a `NodeConfig`.
///
/// We use `NodeConfig::try_parse_from(synthetic_argv)` so we get the same
/// defaulting behavior as the standalone `zerox1-node-enterprise` binary,
/// without consuming the daemon's own CLI args. The role seed is written
/// to `<secrets_dir>/.runtime-keypair-multiply` (raw 32 bytes — matches
/// `AgentIdentity::load_or_generate`'s expected format).
fn build_node_config(args: &RunArgs, role_id: &RoleIdentity) -> Result<NodeConfig> {
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

    NodeConfig::try_parse_from(&argv).map_err(|e| anyhow::anyhow!("synthesizing NodeConfig: {e}"))
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
    monitor_ctx: liq_monitor::LiqMonitorCtx,
    pnl_log_path: PathBuf,
    pnl_start_ts: u64,
    pnl_paper_principal: f64,
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
            Ok(()) => info!(role = %role_id.role().as_str(), nonce = n, "BEACON emitted"),
            Err(e) => warn!(?e, "beacon send failed"),
        }

        // Liquidation-distance monitor — log-only when no orchestrator
        // recipient is configured. Don't fail the loop on a tick error;
        // the next tick may succeed.
        if let Err(e) = liq_monitor::tick(&handle, &monitor_ctx).await {
            warn!(?e, "liq_monitor tick failed");
        }

        // Snapshot pnl + append to log. Errors are warned but never fail
        // the beacon loop — telemetry must not take down the daemon.
        match pnl::snapshot(
            &monitor_ctx.rpc,
            monitor_ctx.user,
            monitor_ctx.lending_market,
            pnl_start_ts,
            pnl_paper_principal,
        )
        .await
        {
            Ok(snap) => {
                if let Err(e) = pnl::append_to_log(&pnl_log_path, &snap) {
                    warn!(?e, "pnl log write failed");
                } else {
                    debug!(
                        net_equity_uusdc = snap.net_equity_uusdc,
                        "pnl snapshot recorded"
                    );
                }
            }
            Err(e) => warn!(?e, "pnl snapshot failed"),
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
    buf.extend_from_slice(&vk); // agent_id (= verifying_key in enterprise mode)
    buf.extend_from_slice(&vk); // verifying_key
    buf.extend_from_slice(name); // display name
    buf
}

/// Initialize tracing. Honors `RUST_LOG_FORMAT=json` to emit structured
/// JSON tracing events (one event per line) so the dashboard server's
/// envelope decoder can parse them. Defaults to the human text formatter.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let json_mode = std::env::var("RUST_LOG_FORMAT")
        .map(|v| v == "json")
        .unwrap_or(false);
    if json_mode {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();
    match args.cmd {
        Cmd::Run(run_args) => run_daemon(run_args),
        Cmd::Report(report_args) => reporter::report(&report_args.log, report_args.since_secs),
    }
}

/// Cross-validate that the RPC URL matches the declared network by querying
/// `getGenesisHash` and comparing against the known mainnet/devnet hashes.
/// Returns Err on mismatch — a hard fail before any chain-touching state is
/// constructed.
async fn verify_network_matches_rpc(network: &str, rpc_url: &str) -> Result<()> {
    const MAINNET_GENESIS: &str = "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d";
    const DEVNET_GENESIS: &str = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG";

    let ctx = RpcContext::new(rpc_url.to_string(), CommitmentConfig::confirmed());
    let genesis: String = ctx
        .client
        .get_genesis_hash()
        .await
        .context("get_genesis_hash")?
        .to_string();

    let expected = match network {
        "mainnet" => MAINNET_GENESIS,
        "devnet" => DEVNET_GENESIS,
        _ => anyhow::bail!("unknown network {:?}", network),
    };
    if genesis != expected {
        anyhow::bail!(
            "RPC URL {} returned genesis hash {} but --network {} expects {}",
            rpc_url,
            genesis,
            network,
            expected
        );
    }
    info!(network, %genesis, "rpc network verified");
    Ok(())
}

fn run_daemon(args: RunArgs) -> Result<()> {
    // Network sanity gates.
    if args.network != "devnet" && args.network != "mainnet" {
        anyhow::bail!(
            "--network must be 'devnet' or 'mainnet', got {:?}",
            args.network
        );
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

    // Parse optional orchestrator agent_id (32-byte hex). When unset, the
    // liquidation monitor logs only — no mesh send. Audit-fix C1: ALSO
    // required on mainnet as the sender allowlist for Assigns. Refuse to
    // boot on mainnet without it.
    let orchestrator_agent_id = parse_optional_pubkey32(
        args.orchestrator_agent_id.as_deref(),
        "--orchestrator-agent-id",
    )?;
    if args.network == "mainnet" && orchestrator_agent_id.is_none() {
        anyhow::bail!(
            "--network mainnet requires --orchestrator-agent-id (audit-fix C1: \
             execution daemons must reject Assign envelopes from any peer other \
             than the configured orchestrator)"
        );
    }

    // riskwatcher M7: Parse optional --riskwatcher pubkey (32-byte hex).
    // `None` disables the soft-veto entirely; Escalates are observed only.
    let riskwatcher_pubkey = parse_optional_pubkey32(args.riskwatcher.as_deref(), "--riskwatcher")?;

    info!(
        network = %args.network,
        rpc_url = %args.rpc_url,
        simulate_only = args.simulate_only,
        require_approval,
        max_position_usdc_lamports = args.max_position_usdc_lamports,
        riskwatcher_configured = riskwatcher_pubkey.is_some(),
        "multiply args validated",
    );

    // Existing multiply boot logic — Wallet/whitelist/journal are kept and
    // augmented with the embedded mesh node, not replaced.
    let wallet = Arc::new(Wallet::load(&args.wallet)?);
    let whitelist = Arc::new(SigningWhitelist::new(kamino::whitelist_program_ids()));
    let journal = journal::Journal::open(&args.journal)?;
    let rpc = Arc::new(RpcContext::new(
        args.rpc_url.clone(),
        CommitmentConfig::confirmed(),
    ));

    let rt = build_runtime(RuntimeProfile::SingleThread)?;
    rt.block_on(async move {
        // Audit-fix I3: cross-validate that the RPC URL matches the declared
        // network. Catches the "declared mainnet but pointed at devnet RPC"
        // typo before any chain work. One extra RPC call at boot.
        verify_network_matches_rpc(&args.network, &args.rpc_url).await?;

        let secrets = FileSource::new(&args.secrets_dir);
        let role_identity = load_role_identity(&secrets, Role::Multiply, "multiply-role.key")
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
            orchestrator_agent_id,
            riskwatcher_pubkey,
        })
        .run()
        .await
    })
}

/// Parse an optional 32-byte pubkey from a hex string. Returns `Ok(None)`
/// when the input is `None`, `Ok(Some(arr))` when the input is exactly
/// 64 hex chars, and `Err` otherwise — the field name is folded into the
/// error context so operators can tell which CLI flag was malformed.
fn parse_optional_pubkey32(value: Option<&str>, field: &'static str) -> Result<Option<[u8; 32]>> {
    let Some(hex_str) = value else {
        return Ok(None);
    };
    let bytes = hex::decode(hex_str).with_context(|| format!("decode {field}: must be hex"))?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "{field} must be 32 bytes (64 hex chars), got {}",
            bytes.len()
        );
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(Some(arr))
}
