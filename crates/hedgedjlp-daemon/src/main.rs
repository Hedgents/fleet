//! hedgedjlp-daemon — fleet's delta-neutral basis trader (long JLP, short
//! Jupiter Perps).
//!
//! M1: minimal scaffold. The daemon parses the smallest set of args
//! required to boot, loads its role identity, builds an embedded
//! `NodeService`, listens on a multiaddr, and emits BEACON envelopes on
//! a shared `Arc<AtomicU64>` nonce. Inbox envelopes are logged at INFO.
//!
//! Subsequent milestones layer on:
//! - M2: hard-coded safety caps (`caps.rs`)
//! - M3: full CLI args, `--network`/genesis-hash gates, `--simulate-only`
//! - M4: approval queue + dispatch loop
//! - M5: devnet sim-only round-trip via fleet-pm-stub
//! - M6: JLP buy leg via Jupiter swap
//! - M7: JLP composition + delta math
//! - M8: Jupiter Perps hedge leg (2-tx request-execute)
//! - M9: periodic rebalancer + borrow-rate watch
//! - M10: telemetry
//! - M11: withdrawal path
//! - M12: mainnet runbook

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{info, warn};

use zerox1_defi_runtime::identity::{Role, RoleIdentity};
use zerox1_defi_runtime::secrets::{load_role_identity, FileSource};

use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::envelope::{Envelope, BROADCAST_RECIPIENT};
use zerox1_protocol::message::MsgType;

#[derive(Parser, Debug)]
#[command(name = "hedgedjlp-daemon", about = "Fleet's delta-neutral basis trader (long JLP, short Jupiter Perps)")]
struct Args {
    /// Subcommand. v0 only supports "run".
    #[arg(long, default_value = "run")]
    subcommand: String,

    /// Directory holding the daemon's role key.
    /// Expected files: hedgedjlp-role.key (32 raw bytes).
    #[arg(long)]
    secrets_dir: PathBuf,

    /// libp2p listen multiaddr.
    #[arg(long, default_value = "/ip4/0.0.0.0/tcp/19311")]
    listen: String,

    /// Bootstrap peer multiaddrs (repeatable).
    #[arg(long)]
    bootstrap: Vec<String>,

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
        anyhow::bail!(
            "--subcommand must be 'run' (v0 only supports run), got {:?}",
            args.subcommand
        );
    }

    info!("hedgedjlp-daemon args validated");

    // Load role key from {secrets_dir}/hedgedjlp-role.key.
    let secrets = FileSource::new(&args.secrets_dir);
    let role_identity =
        load_role_identity(&secrets, Role::HedgedJlp, "hedgedjlp-role.key")
            .await
            .context("loading hedgedjlp role key")?;
    info!(role = %role_identity.role().as_str(), "Loaded identity");

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
        r = handle_inbox(inbox_handle) => {
            warn!(?r, "inbox dispatcher exited");
            r
        }
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
            Ok(())
        }
    }
}

/// Translate `Args` + role seed into a `NodeConfig`.
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

/// M1 inbox handler — log each delivery. Per-MsgType dispatch lands in M4.
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
