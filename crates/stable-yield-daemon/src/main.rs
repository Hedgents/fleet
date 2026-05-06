//! stable-yield-daemon — fleet's passive-supply USDC lender.
//!
//! M3: CLI + boot. The daemon parses args, validates network/cap/ack
//! gates, cross-checks the RPC URL against the known mainnet/devnet
//! genesis hashes, loads its role key + Solana wallet, builds an
//! embedded `NodeService`, listens on the configured multiaddr, and
//! emits BEACON envelopes on a shared `Arc<AtomicU64>` nonce.
//!
//! Inbox dispatch (Assign / Approve handling) lands in M4. Kamino
//! supply ixns land in M6. For M3 the daemon will log incoming
//! envelopes at INFO and discard them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
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

use stable_yield_daemon::{caps, kamino};

#[derive(Parser, Debug)]
#[command(name = "stable-yield-daemon", about = "Fleet's passive-supply USDC lender")]
struct Args {
    /// Subcommand. v0 only supports "run".
    #[arg(long, default_value = "run")]
    subcommand: String,

    /// Directory holding the daemon's role key + Solana wallet.
    /// Expected files: stable-yield-role.key (32 raw bytes), solana-wallet.json.
    #[arg(long)]
    secrets_dir: PathBuf,

    /// libp2p listen multiaddr.
    #[arg(long, default_value = "/ip4/0.0.0.0/tcp/19310")]
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

    /// Sim-only mode bypasses tx submission and only runs simulate_transaction.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    simulate_only: bool,

    /// When true, Assigns are queued and require an Approve envelope before
    /// execution. None defaults to true on mainnet, false on devnet.
    #[arg(long)]
    require_approval: Option<bool>,

    /// CLI ceiling on total USDC the daemon will supply across positions.
    /// Must be ≤ caps::MAX_POSITION_USDC_LAMPORTS.
    /// Default: $5,000 USDC (5e9 lamports).
    #[arg(long, default_value_t = 5_000_000_000)]
    max_position_usdc_lamports: u64,

    /// Beacon emit interval, seconds.
    #[arg(long, default_value_t = 5)]
    beacon_interval_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    if args.subcommand != "run" {
        bail!(
            "--subcommand must be 'run' (v0 only supports run), got {:?}",
            args.subcommand
        );
    }

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

    info!(
        network = %args.network,
        rpc_url = %args.rpc_url,
        simulate_only = args.simulate_only,
        require_approval,
        max_position_usdc_lamports = args.max_position_usdc_lamports,
        "stable-yield args validated",
    );

    // Audit-fix I3: cross-validate that the RPC URL matches the declared
    // network. Catches the "declared mainnet but pointed at devnet RPC"
    // typo before any chain work. One extra RPC call at boot.
    verify_network_matches_rpc(&args.network, &args.rpc_url).await?;

    // Load role key from {secrets_dir}/stable-yield-role.key.
    // The runtime's Role enum doesn't have a dedicated StableYield variant;
    // the closest match for a USDC-supply daemon is StableFloor. The file
    // name is fixed to "stable-yield-role.key" per the M3 spec.
    let secrets = FileSource::new(&args.secrets_dir);
    let role_identity =
        load_role_identity(&secrets, Role::StableFloor, "stable-yield-role.key")
            .await
            .context("loading stable-yield role key")?;
    info!(role = %role_identity.role().as_str(), "Loaded identity");

    // Load Solana wallet from {secrets_dir}/solana-wallet.json.
    let wallet_path = args.secrets_dir.join("solana-wallet.json");
    let wallet = Arc::new(
        Wallet::load(&wallet_path)
            .with_context(|| format!("loading wallet from {}", wallet_path.display()))?,
    );

    // RpcContext for chain reads/sims (M6 will use it for tx building).
    let rpc = Arc::new(RpcContext::new(
        args.rpc_url.clone(),
        CommitmentConfig::confirmed(),
    ));

    // Empty whitelist for M3 — populated in M6 once the lending ixn lands.
    let whitelist = Arc::new(SigningWhitelist::new(kamino::whitelist_program_ids()));

    // Stash these so they're not dropped before NodeService boot. M4 / M6
    // will start using them; the underscore-binding silences unused warnings
    // without losing the references.
    let _ = (wallet, whitelist, rpc, require_approval);

    // Build the embedded node from synthetic argv (avoids consuming the
    // daemon's own argv with NodeConfig::parse()).
    let node_config = build_node_config(&args, &role_identity)?;
    let service = NodeService::build(node_config).await?;
    let handle = service.handle();
    info!(listen = %args.listen, "stable-yield listening");

    // Shared outbound nonce counter for all envelope types (BEACONs, Reports, etc.)
    let outbound_nonce = Arc::new(AtomicU64::new(1));

    let beacon_interval = Duration::from_secs(args.beacon_interval_secs);
    let beacon_handle = handle.clone();
    let beacon_role = role_identity.clone();
    let beacon_nonce = outbound_nonce.clone();

    let inbox_handle = handle.clone();

    tokio::select! {
        r = service.run() => {
            warn!(?r, "node loop exited");
            r
        }
        r = emit_beacons(beacon_handle, beacon_role, beacon_interval, beacon_nonce) => {
            warn!(?r, "beacon emitter exited");
            r
        }
        r = log_inbox(inbox_handle) => {
            warn!(?r, "inbox logger exited");
            r
        }
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
            Ok(())
        }
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
/// to `<secrets_dir>/.runtime-keypair-stable-yield` (raw 32 bytes — matches
/// `AgentIdentity::load_or_generate`'s expected format).
fn build_node_config(args: &Args, role_id: &RoleIdentity) -> Result<NodeConfig> {
    let keypair_path = args.secrets_dir.join(".runtime-keypair-stable-yield");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing keypair to {}", keypair_path.display()))?;

    let mut argv: Vec<String> = vec!["stable-yield".to_string()];
    argv.push("--listen-addr".into());
    argv.push(args.listen.clone());
    argv.push("--keypair-path".into());
    argv.push(keypair_path.display().to_string());
    argv.push("--agent-name".into());
    argv.push("stable-yield".to_string());
    for boot in args.bootstrap.iter().filter(|b| !b.is_empty()) {
        argv.push("--bootstrap".into());
        argv.push(boot.clone());
    }

    NodeConfig::try_parse_from(&argv)
        .map_err(|e| anyhow::anyhow!("synthesizing NodeConfig: {e}"))
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

/// Emit a Beacon envelope onto the mesh every `interval`. Mirror of
/// multiply-daemon's emit_beacons but without the liquidation monitor /
/// pnl snapshot side-channels (those land in M7).
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

/// M3 inbox loop: log incoming envelopes at INFO and discard. Real
/// dispatch (Assign / Approve handling) lands in M4.
async fn log_inbox(mut handle: NodeHandle) -> Result<()> {
    while let Some(env) = handle.recv().await {
        info!(
            msg_type = ?env.msg_type,
            sender = %hex::encode(env.sender),
            nonce = env.nonce,
            "inbox envelope received (M3 stub: discarding)",
        );
    }
    Ok(())
}
