//! Fleet PM stub — sends one Assign envelope and prints the Report.
//!
//! Until the mobile app's PM client is wired up, this is the way to
//! drive the fleet end-to-end during development.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use zerox1_defi_runtime::{
    identity::{Role, RoleIdentity},
    secrets::{FileSource, load_role_identity},
};
use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::{
    envelope::{Envelope, BROADCAST_RECIPIENT},
    fleet::multiply::AssignMultiply,
    fleet::stable_lend::{AssignStableLend, WithdrawStableLend},
    message::MsgType,
};

#[derive(Parser, Debug)]
#[command(about = "Send one Assign envelope to a fleet desk and print the Report.")]
struct Args {
    /// Directory holding the orchestrator's role-key file.
    #[arg(long, env = "ZX_SECRETS_DIR")]
    secrets_dir: PathBuf,
    /// libp2p listen multiaddr for this PM stub.
    #[arg(long, env = "ZX_LISTEN", default_value = "/ip4/0.0.0.0/tcp/0")]
    listen: String,
    /// Bootstrap multiaddrs to dial (comma-sep).
    #[arg(long, env = "ZX_BOOTSTRAP", value_delimiter = ',')]
    bootstrap: Vec<String>,
    /// Timeout (seconds) waiting for a Report after sending the Assign.
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
    /// Recipient agent_id (32-byte hex) for bilateral envelopes (e.g. Assign).
    /// Without this, MsgType::Assign is dropped by the node because broadcast
    /// recipients can't be routed bilaterally. To find a daemon's agent_id,
    /// look at its boot log: "Loaded identity ... agent_id=<hex>".
    #[arg(long)]
    recipient_agent_id: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Send AssignMultiply to the Multiply Desk.
    AssignMultiply {
        /// Target loan-to-value in basis points (6000 = 60%).
        #[arg(long)]
        target_ltv_bps: u16,
        /// Maximum slippage in bps.
        #[arg(long, default_value_t = 50)]
        max_slippage_bps: u16,
        /// Vault key (32-byte hex). Defaults to all-zeros for smoke tests.
        #[arg(long, default_value = "0000000000000000000000000000000000000000000000000000000000000000")]
        vault_hex: String,
    },
    /// Send AssignStableLend to the stable-yield desk.
    AssignStableLend {
        /// Kamino lending market pubkey (base58).
        #[arg(long)]
        market: String,
        /// USDC reserve pubkey within the market (base58).
        #[arg(long)]
        reserve: String,
        /// USDC lamports to deposit (6 decimals — 10_000_000 = $10).
        #[arg(long, default_value_t = 10_000_000)]
        usdc_lamports: u64,
        /// 0 = no deadline.
        #[arg(long, default_value_t = 0)]
        deadline_unix: u64,
    },
    /// Send an Approve envelope referencing a queued Assign by conv_id.
    /// Pair with --recipient-agent-id (the daemon holding the queued Assign).
    Approve {
        /// 16-byte conv_id (32 hex chars) — must match a pending Assign on the daemon.
        #[arg(long)]
        conv_hex: String,
    },
    /// Send WithdrawStableLend to the stable-yield desk.
    WithdrawStableLend {
        /// Kamino lending market pubkey (base58).
        #[arg(long)]
        market: String,
        /// USDC reserve pubkey within the market (base58).
        #[arg(long)]
        reserve: String,
        /// USDC lamports to withdraw (6 decimals — 5_000_000 = $5).
        /// Pass `u64::MAX` (18446744073709551615) for full withdrawal.
        #[arg(long, default_value_t = 5_000_000)]
        usdc_lamports: u64,
        /// 0 = no deadline.
        #[arg(long, default_value_t = 0)]
        deadline_unix: u64,
    },
}

/// Decode a base58-encoded 32-byte pubkey string.
fn decode_b58_pubkey(s: &str, label: &str) -> Result<[u8; 32]> {
    let bytes = bs58::decode(s).into_vec()
        .with_context(|| format!("decode --{label} as base58"))?;
    if bytes.len() != 32 {
        anyhow::bail!("--{label} must decode to 32 bytes (got {})", bytes.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn write_keypair(path: &Path, seed: &[u8; 32]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    let mut f = opts.open(path)?;
    f.write_all(seed)?;
    Ok(())
}

fn build_node_config(args: &Args, role_id: &RoleIdentity) -> Result<NodeConfig> {
    let keypair_path = args.secrets_dir.join(".runtime-keypair-orchestrator");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing keypair to {}", keypair_path.display()))?;

    let mut argv: Vec<String> = vec!["pm-stub".to_string()];
    argv.push("--listen-addr".into());
    argv.push(args.listen.clone());
    argv.push("--keypair-path".into());
    argv.push(keypair_path.display().to_string());
    argv.push("--agent-name".into());
    argv.push("orchestrator".into());
    for boot in args.bootstrap.iter().filter(|b| !b.is_empty()) {
        argv.push("--bootstrap".into());
        argv.push(boot.clone());
    }
    NodeConfig::try_parse_from(&argv)
        .map_err(|e| anyhow::anyhow!("synthesizing NodeConfig: {e}"))
}

async fn wait_for_report_loop(handle: &mut NodeHandle, conv: [u8; 16]) -> Result<Envelope> {
    loop {
        match handle.recv().await {
            Some(env) if env.msg_type == MsgType::Report && env.conversation_id == conv => {
                return Ok(env);
            }
            Some(other) => {
                tracing::debug!(msg_type = ?other.msg_type, "ignoring non-Report envelope");
            }
            None => anyhow::bail!("inbox closed"),
        }
    }
}

fn print_report(report: &Envelope, label: &str) {
    println!("Report received: msg_type={:?} sender={} conv={} label={}",
        report.msg_type,
        hex::encode(report.sender),
        hex::encode(report.conversation_id),
        label,
    );
    // Try to decode the payload using the report type matching the issued
    // command. ReportMultiply and ReportStableLend share a common
    // ReportHeader prefix but the trailing fields differ — try the matching
    // type first, fall through to the other, then to raw hex.
    if label == "AssignStableLend" {
        if let Ok(parsed) = ciborium::de::from_reader::<zerox1_protocol::fleet::stable_lend::ReportStableLend, _>(&report.payload[..]) {
            println!("Report payload (decoded as ReportStableLend): {:?}", parsed);
            println!("deposited_usdc_lamports={} ok={}", parsed.deposited_usdc_lamports, parsed.header.ok);
            return;
        }
    }
    if label == "WithdrawStableLend" {
        if let Ok(parsed) = ciborium::de::from_reader::<zerox1_protocol::fleet::stable_lend::ReportStableWithdraw, _>(&report.payload[..]) {
            println!("Report payload (decoded as ReportStableWithdraw): {:?}", parsed);
            println!("withdrawn_usdc_lamports={} ok={}", parsed.withdrawn_usdc_lamports, parsed.header.ok);
            return;
        }
    }
    if let Ok(parsed) = ciborium::de::from_reader::<zerox1_protocol::fleet::multiply::ReportMultiply, _>(&report.payload[..]) {
        println!("Report payload (decoded as ReportMultiply): {:?}", parsed);
        return;
    }
    if let Ok(parsed) = ciborium::de::from_reader::<zerox1_protocol::fleet::stable_lend::ReportStableLend, _>(&report.payload[..]) {
        println!("Report payload (decoded as ReportStableLend): {:?}", parsed);
        return;
    }
    if let Ok(parsed) = ciborium::de::from_reader::<zerox1_protocol::fleet::stable_lend::ReportStableWithdraw, _>(&report.payload[..]) {
        println!("Report payload (decoded as ReportStableWithdraw): {:?}", parsed);
        return;
    }
    println!("Report payload (raw): {} bytes hex={}",
        report.payload.len(),
        hex::encode(&report.payload));
}

fn make_conversation_id() -> [u8; 16] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut id = [0u8; 16];
    id[..16].copy_from_slice(&nanos.to_be_bytes());
    id
}

async fn send_one_beacon(
    handle: &NodeHandle,
    role_id: &RoleIdentity,
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(role_id.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    // Beacon payload: [agent_id(32)][verifying_key(32)][role_name(utf-8)]
    // Convention: agent_id == verifying_key in enterprise mode.
    let role_name = role_id.role().as_str();
    let mut payload = Vec::with_capacity(32 + 32 + role_name.len());
    payload.extend_from_slice(&sender_pubkey);   // agent_id
    payload.extend_from_slice(&sender_pubkey);   // verifying_key
    payload.extend_from_slice(role_name.as_bytes());

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let env = Envelope::build(
        MsgType::Beacon,
        sender_pubkey,
        BROADCAST_RECIPIENT,
        now_secs,
        0,                       // nonce — single shot
        [0u8; 16],                // no conversation_id for broadcasts
        payload,
        &signing_key,
    );
    handle.send(env).await.context("send beacon")?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Load orchestrator role identity.
    let secrets = FileSource::new(&args.secrets_dir);
    let role_id = load_role_identity(&secrets, Role::Orchestrator, "orchestrator-role.key").await
        .context("load orchestrator role identity")?;
    info!(role = %role_id.role().as_str(), "fleet-pm-stub starting");

    // Build the embedded node and grab a handle BEFORE consuming the service.
    let node_cfg = build_node_config(&args, &role_id)?;
    let service = NodeService::build(node_cfg).await?;
    let mut handle = service.handle();
    let _node_task = tokio::spawn(service.run());

    // Give the node a moment to bind + connect to bootstrap peers.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Decode the recipient. If --recipient-agent-id is omitted, fall back
    // to broadcast (which only works for non-bilateral MsgTypes — Assign
    // will be dropped). Print a warning when falling back.
    let recipient: [u8; 32] = match &args.recipient_agent_id {
        Some(hex) => {
            let bytes = hex::decode(hex).context("decode --recipient-agent-id")?;
            if bytes.len() != 32 {
                anyhow::bail!("--recipient-agent-id must be 32 bytes (64 hex chars), got {}", bytes.len());
            }
            let mut r = [0u8; 32];
            r.copy_from_slice(&bytes);
            r
        }
        None => {
            warn!(
                "no --recipient-agent-id; falling back to BROADCAST_RECIPIENT. \
                 Bilateral envelopes (e.g. Assign) will be dropped. Pass \
                 --recipient-agent-id <hex> from the daemon's boot log."
            );
            BROADCAST_RECIPIENT
        }
    };

    // Determine msg_type, conv_id, and serialized payload based on the command.
    // - AssignMultiply: new conv_id, MsgType::Assign, AssignMultiply CBOR payload.
    // - Approve:        operator-supplied conv_hex, MsgType::Approve, empty payload.
    let (msg_type, conv, payload_bytes, label): (MsgType, [u8; 16], Vec<u8>, &'static str) = match &args.cmd {
        Cmd::AssignMultiply { target_ltv_bps, max_slippage_bps, vault_hex } => {
            let mut vault = [0u8; 32];
            let bytes = hex::decode(vault_hex).context("decode --vault-hex")?;
            if bytes.len() != 32 {
                anyhow::bail!("--vault-hex must be 32 bytes (got {})", bytes.len());
            }
            vault.copy_from_slice(&bytes);

            let assign = AssignMultiply {
                vault,
                target_ltv_bps: *target_ltv_bps,
                max_slippage_bps: *max_slippage_bps,
                deadline_unix: now_unix() + 300,
            };
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&assign, &mut buf)
                .context("serialize AssignMultiply")?;
            (MsgType::Assign, make_conversation_id(), buf, "AssignMultiply")
        }
        Cmd::AssignStableLend { market, reserve, usdc_lamports, deadline_unix } => {
            let market_bytes = decode_b58_pubkey(market, "market")?;
            let reserve_bytes = decode_b58_pubkey(reserve, "reserve")?;
            let assign = AssignStableLend {
                market: market_bytes,
                reserve: reserve_bytes,
                usdc_lamports: *usdc_lamports,
                deadline_unix: *deadline_unix,
            };
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&assign, &mut buf)
                .context("serialize AssignStableLend")?;
            (MsgType::Assign, make_conversation_id(), buf, "AssignStableLend")
        }
        Cmd::Approve { conv_hex } => {
            let bytes = hex::decode(conv_hex).context("decode --conv-hex")?;
            if bytes.len() != 16 {
                anyhow::bail!("--conv-hex must be 16 bytes (32 hex chars), got {}", bytes.len());
            }
            let mut conv = [0u8; 16];
            conv.copy_from_slice(&bytes);
            (MsgType::Approve, conv, Vec::new(), "Approve")
        }
        Cmd::WithdrawStableLend { market, reserve, usdc_lamports, deadline_unix } => {
            let market_bytes = decode_b58_pubkey(market, "market")?;
            let reserve_bytes = decode_b58_pubkey(reserve, "reserve")?;
            let withdraw = WithdrawStableLend {
                market: market_bytes,
                reserve: reserve_bytes,
                usdc_lamports: *usdc_lamports,
                deadline_unix: *deadline_unix,
            };
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&withdraw, &mut buf)
                .context("serialize WithdrawStableLend")?;
            (MsgType::Withdraw, make_conversation_id(), buf, "WithdrawStableLend")
        }
    };

    info!(conv = %hex::encode(conv), label, "conv id selected");

    // Emit one BEACON so peers register our pubkey + role. Without this,
    // the bilateral request-response handler on the receiving daemon will
    // silently drop our envelope (it validates sender identity against
    // peer_states, which is populated by BEACON observations).
    if let Err(e) = send_one_beacon(&handle, &role_id).await {
        warn!(?e, "initial BEACON send failed; recipient may drop our envelope");
    }
    info!(label, "BEACON emitted; waiting for it to propagate before sending");
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Retry-send loop. The recipient's bilateral peer_id may not be in our
    // node's lookup table immediately at boot; their BEACON has to land first.
    // We send, wait briefly for a Report, and re-send if nothing arrives.
    // Use incrementing nonces to avoid replay validation errors.
    let signing_key = ed25519_dalek::SigningKey::from_bytes(role_id.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();
    let total_timeout = Duration::from_secs(args.timeout_secs);
    let retry_interval = Duration::from_secs(3);
    let started = std::time::Instant::now();
    let mut attempt = 0u32;
    let mut next_nonce = 1u64;

    loop {
        attempt += 1;
        // Rebuild the envelope with the current nonce; payload bytes stay the same.
        let env = Envelope::build(
            msg_type,
            sender_pubkey,
            recipient,
            now_unix(),
            next_nonce,
            conv,
            payload_bytes.clone(),
            &signing_key,
        );
        next_nonce = next_nonce.wrapping_add(1);

        if let Err(e) = handle.send(env).await {
            warn!(?e, attempt, "send failed");
        } else {
            info!(attempt, label, "envelope sent");
        }

        let remaining = total_timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let wait = std::cmp::min(retry_interval, remaining);

        // Race a recv against the wait window.
        match tokio::time::timeout(wait, wait_for_report_loop(&mut handle, conv)).await {
            Ok(Ok(report)) => {
                // Print and exit successfully.
                print_report(&report, label);
                return Ok(());
            }
            Ok(Err(e)) => {
                // wait_for_report_loop returned an error (e.g., inbox closed)
                anyhow::bail!("wait_for_report_loop failed: {e}");
            }
            Err(_) => {
                // Timed out within this retry window. Loop and try again.
                info!(attempt, elapsed_secs = started.elapsed().as_secs(), "no Report yet, retrying send");
            }
        }
    }

    eprintln!("No Report received: timed out after {:?} ({} attempts)", total_timeout, attempt);
    std::process::exit(2);
}
