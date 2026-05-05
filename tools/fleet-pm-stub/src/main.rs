//! Fleet PM stub — sends one Assign envelope and prints the Report.
//!
//! Until the mobile app's PM client is wired up, this is the way to
//! drive the fleet end-to-end during development.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::info;
use zerox1_defi_runtime::{
    identity::{Role, RoleIdentity},
    secrets::{FileSource, load_role_identity},
};
use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::{
    envelope::{Envelope, BROADCAST_RECIPIENT},
    fleet::multiply::AssignMultiply,
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

fn build_assign_envelope<T: serde::Serialize>(
    msg_type: MsgType,
    role_id: &RoleIdentity,
    nonce: u64,
    conversation_id: [u8; 16],
    target_role: Role,
    payload: T,
) -> Result<Envelope> {
    // For a unicast Assign, recipient should be the target desk's verifying-key
    // bytes — but in this scaffold we don't yet resolve roles via a registry.
    // For now, broadcast: every daemon receives, the target role's daemon
    // dispatches by the inner payload type. The strategy plan upgrades this
    // to a proper resolve via runtime::role_registry.
    let _ = target_role;  // unused for now; placeholder for the resolve step

    let signing_key = ed25519_dalek::SigningKey::from_bytes(role_id.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    let mut payload_bytes = Vec::new();
    ciborium::ser::into_writer(&payload, &mut payload_bytes)
        .context("serialize payload to CBOR")?;

    Ok(Envelope::build(
        msg_type,
        sender_pubkey,
        BROADCAST_RECIPIENT,
        now_unix(),
        nonce,
        conversation_id,
        payload_bytes,
        &signing_key,
    ))
}

async fn wait_for_report(handle: &mut NodeHandle, timeout: Duration, conv: [u8; 16]) -> Result<Envelope> {
    let res = tokio::time::timeout(timeout, async {
        loop {
            match handle.recv().await {
                Some(env) if env.msg_type == MsgType::Report && env.conversation_id == conv => {
                    return Ok::<Envelope, anyhow::Error>(env);
                }
                Some(other) => {
                    tracing::debug!(msg_type = ?other.msg_type, "ignoring non-Report envelope");
                }
                None => anyhow::bail!("inbox closed"),
            }
        }
    }).await;
    match res {
        Ok(r) => r,
        Err(_) => anyhow::bail!("timed out after {:?} waiting for Report", timeout),
    }
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

    // Construct + send the Assign.
    let conv = make_conversation_id();
    match args.cmd {
        Cmd::AssignMultiply { target_ltv_bps, max_slippage_bps, ref vault_hex } => {
            let mut vault = [0u8; 32];
            let bytes = hex::decode(vault_hex).context("decode --vault-hex")?;
            if bytes.len() != 32 {
                anyhow::bail!("--vault-hex must be 32 bytes (got {})", bytes.len());
            }
            vault.copy_from_slice(&bytes);

            let payload = AssignMultiply {
                vault,
                target_ltv_bps,
                max_slippage_bps,
                deadline_unix: now_unix() + 300,
            };
            let env = build_assign_envelope(
                MsgType::Assign,
                &role_id,
                1,
                conv,
                Role::Multiply,
                payload,
            )?;

            info!(target = "multiply", target_ltv_bps, "sending AssignMultiply");
            handle.send(env).await.context("send Assign")?;
        }
    }

    // Wait for the Report.
    let timeout = Duration::from_secs(args.timeout_secs);
    match wait_for_report(&mut handle, timeout, conv).await {
        Ok(report) => {
            println!("Report received: msg_type={:?} sender={} conv={}",
                report.msg_type,
                hex::encode(report.sender),
                hex::encode(report.conversation_id),
            );
            // Try to decode the payload as multiply::ReportMultiply for nicer printing.
            // If it doesn't decode (because the daemon hasn't yet wired strategy
            // dispatch), fall back to printing raw payload bytes.
            match ciborium::de::from_reader::<zerox1_protocol::fleet::multiply::ReportMultiply, _>(&report.payload[..]) {
                Ok(parsed) => println!("Report payload (decoded): {:?}", parsed),
                Err(_) => println!("Report payload (raw): {} bytes hex={}",
                    report.payload.len(),
                    hex::encode(&report.payload)),
            }
        }
        Err(e) => {
            eprintln!("No Report received: {e}");
            std::process::exit(2);
        }
    }

    Ok(())
}
