//! hedgedjlp-daemon — fleet's delta-neutral basis trader (long JLP, short
//! Jupiter Perps).
//!
//! M3: full CLI args + boot + network/genesis-hash gates. The daemon
//! parses args, validates network/cap/ack gates, cross-checks the RPC
//! URL against the known mainnet/devnet genesis hashes, loads its role
//! key + Solana wallet, builds an embedded `NodeService`, listens on
//! the configured multiaddr, and emits BEACON envelopes on a shared
//! `Arc<AtomicU64>` nonce.
//!
//! Inbox dispatch (Assign / Approve handling) lands in M4. JLP buy
//! ixns land in M6. Jupiter Perps hedge ixns land in M8. The periodic
//! rebalancer (using `--rebalance-interval-secs`) wires up in M9.
//! For M3 the daemon will log incoming envelopes at INFO and discard
//! them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::Parser;
use solana_sdk::commitment_config::CommitmentConfig;
use tracing::{info, warn};

use zerox1_defi_runtime::identity::{Role, RoleIdentity};
use zerox1_defi_runtime::rpc::RpcContext;
use zerox1_defi_runtime::secrets::{load_role_identity, FileSource};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};

use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

use hedgedjlp_daemon::{approval, caps, dispatch, rebalance, telemetry, whitelist};

#[derive(Parser, Debug)]
#[command(
    name = "hedgedjlp-daemon",
    about = "Fleet's delta-neutral basis trader (JLP + Jupiter Perps shorts)"
)]
struct Args {
    /// Directory holding the daemon's role key + Solana wallet.
    /// Expected: hedgedjlp-role.key (32 raw bytes), solana-wallet.json.
    #[arg(long)]
    secrets_dir: PathBuf,

    /// libp2p listen multiaddr.
    #[arg(long, default_value = "/ip4/0.0.0.0/tcp/19311")]
    listen: String,

    /// Bootstrap peer multiaddrs (repeatable).
    #[arg(long)]
    bootstrap: Vec<String>,

    /// Solana RPC URL.
    #[arg(long, default_value = "https://api.devnet.solana.com")]
    rpc_url: String,

    /// Must be exactly "devnet" or "mainnet".
    #[arg(long, default_value = "devnet")]
    network: String,

    /// Required ack flag for mainnet — bails if --network=mainnet without this.
    #[arg(long, default_value_t = false)]
    i_understand_this_is_mainnet: bool,

    /// Sim-only: no real position opens. Default true; explicit set false to broadcast.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    simulate_only: bool,

    /// When true, Assigns are queued and require an Approve envelope before
    /// execution. None defaults to true on mainnet, false on devnet.
    #[arg(long)]
    require_approval: Option<bool>,

    /// CLI ceiling on total USDC the daemon will deploy.
    /// Must be ≤ caps::MAX_POSITION_USDC_LAMPORTS ($5M).
    /// Default: $1,000 USDC (1e9 lamports).
    #[arg(long, default_value_t = 1_000_000_000)]
    max_position_usdc_lamports: u64,

    /// Beacon emit interval, seconds.
    #[arg(long, default_value_t = 5)]
    beacon_interval_secs: u64,

    /// Periodic rebalancer interval in seconds. Default 10 min.
    /// M9 wires this into the rebalancer task; for M3 it's parsed and
    /// logged but otherwise inert.
    #[arg(long, default_value_t = 600)]
    rebalance_interval_secs: u64,

    /// Telemetry log path (JSONL, 0600 perms on Unix). One line is
    /// appended per `--telemetry-interval-secs` tick.
    #[arg(long, default_value = "hedgedjlp-pnl.jsonl")]
    telemetry_log: PathBuf,

    /// Telemetry poll interval in seconds. Default 60s.
    #[arg(long, default_value_t = 60)]
    telemetry_interval_secs: u64,

    /// Audit-fix C1: 32-byte hex pubkey of the orchestrator allowed to send
    /// Assign / Withdraw envelopes. Required on `--network=mainnet`.
    /// When unset (devnet sandbox), the sender allowlist is disabled.
    #[arg(long)]
    orchestrator_agent_id: Option<String>,

    /// Paper-trading notional principal in USDC lamports.
    #[arg(long, default_value_t = 1_000_000_000)]
    paper_principal_usdc_lamports: u64,
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

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let args = Args::parse();

    // Network whitelist.
    if args.network != "devnet" && args.network != "mainnet" {
        bail!(
            "--network must be 'devnet' or 'mainnet', got {:?}",
            args.network
        );
    }

    // Mainnet ack gate.
    if args.network == "mainnet" && !args.i_understand_this_is_mainnet {
        bail!(
            "--network=mainnet requires --i-understand-this-is-mainnet flag \
             (this exists to make mainnet promotion explicit)"
        );
    }

    // Cap upper bound.
    if args.max_position_usdc_lamports > caps::MAX_POSITION_USDC_LAMPORTS {
        bail!(
            "--max-position-usdc-lamports {} exceeds compile-time cap {}",
            args.max_position_usdc_lamports,
            caps::MAX_POSITION_USDC_LAMPORTS
        );
    }

    // Default require_approval per-network: true on mainnet, false on devnet.
    let require_approval = args.require_approval.unwrap_or(args.network == "mainnet");

    // Audit-fix C1: parse + enforce the orchestrator allowlist. Mainnet
    // refuses to boot without --orchestrator-agent-id; devnet leaves it
    // optional so the paper-trade-loop continues to work unchanged.
    let orchestrator_agent_id = parse_optional_pubkey32(
        args.orchestrator_agent_id.as_deref(),
        "--orchestrator-agent-id",
    )?;
    if args.network == "mainnet" && orchestrator_agent_id.is_none() {
        bail!(
            "--network mainnet requires --orchestrator-agent-id (audit-fix C1: \
             execution daemons must reject Assign/Withdraw envelopes from any \
             peer other than the configured orchestrator)"
        );
    }

    info!(
        network = %args.network,
        rpc_url = %args.rpc_url,
        simulate_only = args.simulate_only,
        require_approval,
        max_position_usdc_lamports = args.max_position_usdc_lamports,
        rebalance_interval_secs = args.rebalance_interval_secs,
        "hedgedjlp args validated",
    );

    // Audit-fix I3 carry: cross-validate that the RPC URL matches the
    // declared network. Catches the "declared mainnet but pointed at
    // devnet RPC" typo before any chain work.
    verify_network_matches_rpc(&args.network, &args.rpc_url).await?;

    // Load role key from {secrets_dir}/hedgedjlp-role.key.
    let secrets = FileSource::new(&args.secrets_dir);
    let role_identity = load_role_identity(&secrets, Role::HedgedJlp, "hedgedjlp-role.key")
        .await
        .context("loading hedgedjlp role key")?;
    info!(role = %role_identity.role().as_str(), "Loaded identity");

    // Load Solana wallet from {secrets_dir}/solana-wallet.json.
    let wallet_path = args.secrets_dir.join("solana-wallet.json");
    let wallet = Arc::new(
        Wallet::load(&wallet_path)
            .with_context(|| format!("loading wallet from {}", wallet_path.display()))?,
    );

    // RpcContext for chain reads/sims (M6/M8 will use it for tx building).
    let rpc = Arc::new(RpcContext::new(
        args.rpc_url.clone(),
        CommitmentConfig::confirmed(),
    ));

    // Empty whitelist for M4 — populated in M6 (Jupiter swap) + M8 (Jupiter Perps).
    let whitelist = Arc::new(SigningWhitelist::new(whitelist::whitelist_program_ids()));

    // Build the embedded node from synthetic argv.
    let node_config = build_node_config(&args, &role_identity)?;
    let service = NodeService::build(node_config).await?;
    let handle = service.handle();
    info!(listen = %args.listen, "hedgedjlp listening");

    // Shared outbound nonce counter for all envelope types.
    let outbound_nonce = Arc::new(AtomicU64::new(1));

    let beacon_interval = Duration::from_secs(args.beacon_interval_secs);
    let beacon_handle = handle.clone();
    let beacon_role = role_identity.clone();
    let beacon_nonce = outbound_nonce.clone();

    // M9: shared rebalancer state, optionally wired into the dispatch
    // path (M10+) when it lands recording. For M9 v0 the state stays
    // empty (no Active position) — the rebalance loop logs that and
    // no-ops on each tick.
    let rebalance_state = Arc::new(rebalance::RebalanceState::new());
    let rebalance_handle = handle.clone();
    let rebalance_role = role_identity.clone();
    let rebalance_nonce = outbound_nonce.clone();
    let rebalance_rpc = rpc.clone();
    let rebalance_state_run = rebalance_state.clone();
    let rebalance_interval = Duration::from_secs(args.rebalance_interval_secs);

    // M10: telemetry task. Polls the same RebalanceState as the
    // rebalancer and writes one JSONL line per tick.
    let telemetry_rpc = rpc.clone();
    let telemetry_state = rebalance_state.clone();
    let telemetry_log = args.telemetry_log.clone();
    let telemetry_interval_secs = args.telemetry_interval_secs;
    let telemetry_paper_principal = args.paper_principal_usdc_lamports as f64 / 1_000_000.0;
    let telemetry_start_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // M4: build DispatchCtx + spawn dispatch loop alongside BEACON.
    let dispatch_ctx = dispatch::DispatchCtx {
        rpc: rpc.clone(),
        wallet: wallet.clone(),
        whitelist: whitelist.clone(),
        role_identity: role_identity.clone(),
        simulate_only: args.simulate_only,
        require_approval,
        nonce: outbound_nonce.clone(),
        args_max_position_usdc_lamports: args.max_position_usdc_lamports,
        assign_queue: Arc::new(approval::AssignApprovalQueue::new()),
        withdraw_queue: Arc::new(approval::WithdrawApprovalQueue::new()),
        state: rebalance_state.clone(),
        orchestrator_agent_id,
    };
    let dispatch_handle = handle.clone();

    if args.rebalance_interval_secs == 0 {
        info!("--rebalance-interval-secs=0 — rebalancer disabled");
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
                warn!(?r, "dispatch loop exited");
                r
            }
            _ = telemetry::run(
                telemetry_rpc,
                telemetry_state,
                telemetry_log,
                telemetry_interval_secs,
                telemetry_start_ts,
                telemetry_paper_principal,
            ) => {
                warn!("telemetry loop exited");
                Ok(())
            }
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received, shutting down");
                Ok(())
            }
        }
    } else {
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
                warn!(?r, "dispatch loop exited");
                r
            }
            _ = rebalance::run(
                rebalance_rpc,
                rebalance_handle,
                rebalance_role,
                rebalance_nonce,
                rebalance_state_run,
                rebalance_interval,
            ) => {
                warn!("rebalance loop exited");
                Ok(())
            }
            _ = telemetry::run(
                telemetry_rpc,
                telemetry_state,
                telemetry_log,
                telemetry_interval_secs,
                telemetry_start_ts,
                telemetry_paper_principal,
            ) => {
                warn!("telemetry loop exited");
                Ok(())
            }
            _ = tokio::signal::ctrl_c() => {
                info!("ctrl-c received, shutting down");
                Ok(())
            }
        }
    }
}

/// Cross-validate that the RPC URL matches the declared network by querying
/// `getGenesisHash` and comparing against the known mainnet/devnet hashes.
/// Returns Err on mismatch — a hard fail before any chain-touching state is
/// constructed. Lifted verbatim from stable-yield-daemon (audit-fix I3).
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
        _ => bail!("unknown network {:?}", network),
    };
    if genesis != expected {
        bail!(
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

/// Translate `Args` + role seed into a `NodeConfig`.
///
/// Uses `NodeConfig::try_parse_from(synthetic_argv)` so we get the same
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
    argv.push("hedgedjlp".to_string());
    for boot in args.bootstrap.iter().filter(|b| !b.is_empty()) {
        argv.push("--bootstrap".into());
        argv.push(boot.clone());
    }

    NodeConfig::try_parse_from(&argv).map_err(|e| anyhow::anyhow!("synthesizing NodeConfig: {e}"))
}

/// Write a 32-byte Ed25519 seed to `path` in the raw format expected by
/// `AgentIdentity::load_or_generate`.
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
            Ok(()) => info!(role = %role_id.role().as_str(), nonce = n, "BEACON emitted"),
            Err(e) => warn!(?e, "BEACON send failed"),
        }

        tokio::time::sleep(interval).await;
    }
}

/// Parse an optional 32-byte pubkey from a hex string. `None` in → `None` out;
/// 64 hex chars → `Some([u8; 32])`; anything else → `Err`. Mirrors the helper
/// in multiply-daemon/src/main.rs (audit-fix C1).
fn parse_optional_pubkey32(value: Option<&str>, field: &'static str) -> Result<Option<[u8; 32]>> {
    let Some(hex_str) = value else {
        return Ok(None);
    };
    let bytes = hex::decode(hex_str).with_context(|| format!("decode {field}: must be hex"))?;
    if bytes.len() != 32 {
        bail!(
            "{field} must be 32 bytes (64 hex chars), got {}",
            bytes.len()
        );
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&bytes);
    Ok(Some(arr))
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
