//! Fleet PM stub — sends one Assign envelope and prints the Report.
//!
//! Until the mobile app's PM client is wired up, this is the way to
//! drive the fleet end-to-end during development.

use fleet_pm_stub::{allocator, allocator_runner};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use zerox1_defi_runtime::{
    identity::{Role, RoleIdentity},
    secrets::{load_role_identity, FileSource},
};
use zerox1_node_enterprise::{NodeConfig, NodeHandle, NodeService};
use zerox1_protocol::{
    envelope::{Envelope, BROADCAST_RECIPIENT},
    fleet::hedgedjlp::{AssignHedgedJlp, WithdrawHedgedJlp},
    fleet::multiply::{AssignMultiply, WithdrawMultiply},
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
        #[arg(
            long,
            default_value = "0000000000000000000000000000000000000000000000000000000000000000"
        )]
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
    /// Send AssignHedgedJlp to the hedgedjlp desk.
    AssignHedgedjlp {
        /// USDC lamports to deploy across both legs.
        #[arg(long, default_value_t = 200_000_000)]
        usdc_lamports: u64,
        /// Target delta in bps. 0 = neutral.
        #[arg(long, default_value_t = 0)]
        target_delta_bps: i16,
        /// Auto-unwind borrow rate ceiling (bps). Default 5000 = 50% APR.
        #[arg(long, default_value_t = 5_000)]
        max_borrow_rate_bps: u16,
        /// Hard deadline (unix). 0 = no deadline.
        #[arg(long, default_value_t = 0)]
        deadline_unix: u64,
    },
    /// Send WithdrawHedgedJlp to the hedgedjlp desk.
    WithdrawHedgedjlp {
        /// JLP lamports to redeem. u64::MAX = full withdraw.
        #[arg(long)]
        jlp_lamports: u64,
        #[arg(long, default_value_t = 0)]
        deadline_unix: u64,
    },
    /// Regime-aware allocator — pulls live fleet state from the dashboard
    /// REST API and emits one recommendation (NoAction / Withdraw /
    /// Deposit). Dry-run by default; pass `--execute` to also send the
    /// equivalent Assign/Withdraw envelopes to each desk.
    ///
    /// `--execute` requires `--targets-json` describing each strategy's
    /// recipient_agent_id (and, for stable_yield, the Kamino market +
    /// reserve pubkeys). Dry-run needs no targets file.
    Allocator {
        /// Base URL of the dashboard REST API.
        #[arg(long, default_value = "http://127.0.0.1:7700")]
        api_base: String,
        #[arg(long, default_value_t = 200)]
        risk_premium_multiply_bps: i32,
        #[arg(long, default_value_t = 300)]
        risk_premium_hedgedjlp_bps: i32,
        #[arg(long, default_value_t = 5.0)]
        min_action_usd: f64,
        #[arg(long, default_value_t = 0.5)]
        max_action_fraction: f64,
        /// Send the recommended Assign/Withdraw envelope(s). Off by
        /// default — the allocator just prints its recommendation.
        #[arg(long, default_value_t = false)]
        execute: bool,
        /// JSON file with per-strategy recipient_agent_id (+ Kamino
        /// market/reserve pubkeys for stable_yield). Required with
        /// `--execute`.
        #[arg(long)]
        targets_json: Option<PathBuf>,
        /// JSONL audit log path (one record per allocator tick).
        #[arg(long, default_value = "/var/lib/hedgents/logs/allocator-audit.jsonl")]
        audit_log: PathBuf,
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
    /// Send WithdrawMultiply to the Multiply Desk (full lever-down).
    ///
    /// The multiply daemon fully unwinds the leveraged jitoSOL position
    /// (no amount — always 100%). Funds default to the daemon's wallet
    /// unless `--emergency-destination` was configured at daemon boot.
    WithdrawMultiply {
        /// Vault key (32-byte hex). Defaults to all-ones for smoke tests —
        /// the daemon validates non-zero defensively but otherwise
        /// ignores the value (single-vault per daemon).
        #[arg(
            long,
            default_value = "0101010101010101010101010101010101010101010101010101010101010101"
        )]
        vault_hex: String,
        /// Max slippage on the jitoSOL→SOL swap leg, in bps. Capped at
        /// `caps::MAX_SLIPPAGE_BPS` (200) at the daemon side.
        #[arg(long, default_value_t = 100)]
        max_slippage_bps: u16,
        /// 0 = no deadline. Otherwise UNIX-seconds.
        #[arg(long, default_value_t = 0)]
        deadline_unix: u64,
    },
}

/// Decode a base58-encoded 32-byte pubkey string.
fn decode_b58_pubkey(s: &str, label: &str) -> Result<[u8; 32]> {
    let bytes = bs58::decode(s)
        .into_vec()
        .with_context(|| format!("decode --{label} as base58"))?;
    if bytes.len() != 32 {
        anyhow::bail!("--{label} must decode to 32 bytes (got {})", bytes.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    // Use the orchestrator role key as the libp2p P2P identity so that
    // IDENTIFY advertises the same Ed25519 key that signs envelopes.
    // Receiving nodes (e.g. stable-yield) then learn the signing VK via
    // IDENTIFY immediately — no gossipsub BEACON needed. This also lets
    // wait_for_peer() on the pm-stub side resolve via the IDENTIFY-based
    // ApiState update added to the enterprise node.
    let keypair_path = args.secrets_dir.join(".runtime-keypair-orchestrator");
    write_keypair(&keypair_path, role_id.signing_key_bytes())
        .with_context(|| format!("writing orchestrator keypair to {}", keypair_path.display()))?;

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
    NodeConfig::try_parse_from(&argv).map_err(|e| anyhow::anyhow!("synthesizing NodeConfig: {e}"))
}

async fn wait_for_report_loop(
    handle: &mut NodeHandle,
    conv: [u8; 16],
    expected_sender: Option<[u8; 32]>,
    expected_report: ExpectedReport,
    mismatched_reports: &mut Vec<Envelope>,
) -> Result<Envelope> {
    loop {
        match handle.recv().await {
            Some(env) if env.msg_type == MsgType::Report && env.conversation_id == conv => {
                if let Some(expected) = expected_sender {
                    if env.sender != expected {
                        tracing::info!(
                            sender = %hex::encode(env.sender),
                            "ignoring Report from unexpected sender (waiting for recipient)"
                        );
                        continue;
                    }
                }
                // Fix 3b (2026-05-13): require the Report payload to
                // decode cleanly as the type we asked for. Without this,
                // an unrelated daemon that mistakenly responded (e.g.
                // hedgedjlp returning ReportHedgedJlp for an
                // AssignStableLend) short-circuited the wait with a
                // garbage payload.
                if !try_decode_expected(expected_report, &env.payload[..]) {
                    tracing::warn!(
                        sender = %hex::encode(env.sender),
                        expected = ?expected_report,
                        payload_len = env.payload.len(),
                        "Report payload does not match expected type; ignoring \
                         (probably from an unrelated daemon that shouldn't have responded)"
                    );
                    mismatched_reports.push(env);
                    continue;
                }
                return Ok(env);
            }
            Some(other) => {
                tracing::debug!(msg_type = ?other.msg_type, "ignoring non-Report envelope");
            }
            None => anyhow::bail!("inbox closed"),
        }
    }
}

/// The expected Report payload type for a given Assign/Withdraw label.
///
/// Decoupling the table here from `print_report` lets fleet-pm-stub's
/// retry loop reject Reports whose payload doesn't match what we asked
/// for — see `try_decode_expected`. Previously the stub took ANY Report
/// with a matching conversation_id, even if the payload was an unrelated
/// daemon's error Report. That confused us for ~30 min on 2026-05-13
/// when hedgedjlp returned ok=false to an AssignStableLend.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ExpectedReport {
    StableLend,
    StableWithdraw,
    Multiply,
    MultiplyWithdraw,
    HedgedJlp,
    HedgedJlpWithdraw,
    /// Unknown command label — fall through to the raw-hex print.
    Unknown,
}

fn expected_report_for_label(label: &str) -> ExpectedReport {
    match label {
        "AssignStableLend" => ExpectedReport::StableLend,
        "WithdrawStableLend" => ExpectedReport::StableWithdraw,
        "AssignMultiply" => ExpectedReport::Multiply,
        "WithdrawMultiply" => ExpectedReport::MultiplyWithdraw,
        "AssignHedgedJlp" => ExpectedReport::HedgedJlp,
        "WithdrawHedgedJlp" => ExpectedReport::HedgedJlpWithdraw,
        // Approve is fire-and-forget — no Report shape associated.
        _ => ExpectedReport::Unknown,
    }
}

/// Returns `true` if `bytes` cleanly CBOR-decodes as the report type the
/// caller asked for. The fleet-pm-stub uses this to filter Reports during
/// the retry loop so unrelated daemons that mistakenly responded to our
/// Assign don't short-circuit the wait.
fn try_decode_expected(expected: ExpectedReport, bytes: &[u8]) -> bool {
    match expected {
        ExpectedReport::StableLend => ciborium::de::from_reader::<
            zerox1_protocol::fleet::stable_lend::ReportStableLend,
            _,
        >(bytes)
        .is_ok(),
        ExpectedReport::StableWithdraw => ciborium::de::from_reader::<
            zerox1_protocol::fleet::stable_lend::ReportStableWithdraw,
            _,
        >(bytes)
        .is_ok(),
        ExpectedReport::Multiply => {
            ciborium::de::from_reader::<zerox1_protocol::fleet::multiply::ReportMultiply, _>(bytes)
                .is_ok()
        }
        ExpectedReport::MultiplyWithdraw => ciborium::de::from_reader::<
            zerox1_protocol::fleet::multiply::ReportMultiplyWithdraw,
            _,
        >(bytes)
        .is_ok(),
        ExpectedReport::HedgedJlp => ciborium::de::from_reader::<
            zerox1_protocol::fleet::hedgedjlp::ReportHedgedJlp,
            _,
        >(bytes)
        .is_ok(),
        ExpectedReport::HedgedJlpWithdraw => ciborium::de::from_reader::<
            zerox1_protocol::fleet::hedgedjlp::ReportHedgedJlpWithdraw,
            _,
        >(bytes)
        .is_ok(),
        ExpectedReport::Unknown => true,
    }
}

fn print_report(report: &Envelope, label: &str) {
    println!(
        "Report received: msg_type={:?} sender={} conv={} label={}",
        report.msg_type,
        hex::encode(report.sender),
        hex::encode(report.conversation_id),
        label,
    );
    // Decode strictly based on the command label. No fall-through to other
    // decoder types: in CBOR, ReportStableLend and ReportHedgedJlpWithdraw
    // share an outer struct shape, so the first-match-wins ladder used to
    // mis-decode ReportStableLend as ReportHedgedJlpWithdraw (with
    // usdc_returned_lamports=0) when the wrong daemon replied.
    match expected_report_for_label(label) {
        ExpectedReport::StableLend => {
            if let Ok(parsed) = ciborium::de::from_reader::<
                zerox1_protocol::fleet::stable_lend::ReportStableLend,
                _,
            >(&report.payload[..])
            {
                println!("Report payload (decoded as ReportStableLend): {:?}", parsed);
                println!(
                    "deposited_usdc_lamports={} ok={}",
                    parsed.deposited_usdc_lamports, parsed.header.ok
                );
                return;
            }
        }
        ExpectedReport::StableWithdraw => {
            if let Ok(parsed) = ciborium::de::from_reader::<
                zerox1_protocol::fleet::stable_lend::ReportStableWithdraw,
                _,
            >(&report.payload[..])
            {
                println!(
                    "Report payload (decoded as ReportStableWithdraw): {:?}",
                    parsed
                );
                println!(
                    "withdrawn_usdc_lamports={} ok={}",
                    parsed.withdrawn_usdc_lamports, parsed.header.ok
                );
                return;
            }
        }
        ExpectedReport::Multiply => {
            if let Ok(parsed) = ciborium::de::from_reader::<
                zerox1_protocol::fleet::multiply::ReportMultiply,
                _,
            >(&report.payload[..])
            {
                println!("Report payload (decoded as ReportMultiply): {:?}", parsed);
                println!(
                    "resulting_ltv_bps={} ok={}",
                    parsed.resulting_ltv_bps, parsed.header.ok
                );
                return;
            }
        }
        ExpectedReport::MultiplyWithdraw => {
            if let Ok(parsed) = ciborium::de::from_reader::<
                zerox1_protocol::fleet::multiply::ReportMultiplyWithdraw,
                _,
            >(&report.payload[..])
            {
                println!(
                    "Report payload (decoded as ReportMultiplyWithdraw): {:?}",
                    parsed
                );
                println!(
                    "final_usdc_lamports={} residual_sol_lamports={} \
                     tx_signatures_count={} ok={} error_code={:?}",
                    parsed.final_usdc_lamports,
                    parsed.residual_sol_lamports,
                    parsed.tx_signatures.len(),
                    parsed.header.ok,
                    parsed.header.error_code,
                );
                for (i, sig) in parsed.tx_signatures.iter().enumerate() {
                    println!("  tx[{}] = {}", i, sig);
                }
                return;
            }
        }
        ExpectedReport::HedgedJlp => {
            if let Ok(parsed) = ciborium::de::from_reader::<
                zerox1_protocol::fleet::hedgedjlp::ReportHedgedJlp,
                _,
            >(&report.payload[..])
            {
                println!("Report payload (decoded as ReportHedgedJlp): {:?}", parsed);
                println!(
                    "jlp_acquired_lamports={} hedge_notional_usdc={} current_delta_bps={} ok={}",
                    parsed.jlp_acquired_lamports,
                    parsed.hedge_notional_usdc,
                    parsed.current_delta_bps,
                    parsed.header.ok,
                );
                return;
            }
        }
        ExpectedReport::HedgedJlpWithdraw => {
            if let Ok(parsed) = ciborium::de::from_reader::<
                zerox1_protocol::fleet::hedgedjlp::ReportHedgedJlpWithdraw,
                _,
            >(&report.payload[..])
            {
                println!(
                    "Report payload (decoded as ReportHedgedJlpWithdraw): {:?}",
                    parsed
                );
                println!(
                    "usdc_returned_lamports={} ok={}",
                    parsed.usdc_returned_lamports, parsed.header.ok,
                );
                return;
            }
        }
        ExpectedReport::Unknown => {}
    }
    println!(
        "Report payload (raw): {} bytes hex={}",
        report.payload.len(),
        hex::encode(&report.payload)
    );
}

#[cfg(test)]
mod label_dispatch_tests {
    //! Audit of the label → expected-report-type mapping. Pure table
    //! lookup; if this drifts vs. fleet protocol types, the runtime check
    //! in try_decode_expected catches it (silently filters the Report).
    use super::*;

    #[test]
    fn known_labels_resolve_correctly() {
        assert_eq!(
            expected_report_for_label("AssignStableLend"),
            ExpectedReport::StableLend
        );
        assert_eq!(
            expected_report_for_label("WithdrawStableLend"),
            ExpectedReport::StableWithdraw
        );
        assert_eq!(
            expected_report_for_label("AssignMultiply"),
            ExpectedReport::Multiply
        );
        assert_eq!(
            expected_report_for_label("AssignHedgedJlp"),
            ExpectedReport::HedgedJlp
        );
        assert_eq!(
            expected_report_for_label("WithdrawHedgedJlp"),
            ExpectedReport::HedgedJlpWithdraw
        );
        assert_eq!(
            expected_report_for_label("WithdrawMultiply"),
            ExpectedReport::MultiplyWithdraw
        );
    }

    #[test]
    fn unknown_label_falls_through() {
        assert_eq!(
            expected_report_for_label("Approve"),
            ExpectedReport::Unknown
        );
        assert_eq!(
            expected_report_for_label("nonsense"),
            ExpectedReport::Unknown
        );
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

async fn send_one_beacon(handle: &NodeHandle, role_id: &RoleIdentity) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(role_id.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();

    // Beacon payload: [agent_id(32)][verifying_key(32)][role_name(utf-8)]
    // Convention: agent_id == verifying_key in enterprise mode.
    let role_name = role_id.role().as_str();
    let mut payload = Vec::with_capacity(32 + 32 + role_name.len());
    payload.extend_from_slice(&sender_pubkey); // agent_id
    payload.extend_from_slice(&sender_pubkey); // verifying_key
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
        0,         // nonce — single shot
        [0u8; 16], // no conversation_id for broadcasts
        payload,
        &signing_key,
    );
    handle.send(env).await.context("send beacon")?;
    Ok(())
}

/// Build the `(msg_type, conv_id, payload_bytes, label)` tuple for a
/// given `Cmd`. Extracted so both the standard subcommand path and the
/// allocator's `--execute` path can synthesise the same envelopes
/// without code duplication.
fn build_envelope_from_cmd(cmd: &Cmd) -> Result<(MsgType, [u8; 16], Vec<u8>, &'static str)> {
    Ok(match cmd {
        Cmd::AssignMultiply {
            target_ltv_bps,
            max_slippage_bps,
            vault_hex,
        } => {
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
            ciborium::ser::into_writer(&assign, &mut buf).context("serialize AssignMultiply")?;
            (
                MsgType::Assign,
                make_conversation_id(),
                buf,
                "AssignMultiply",
            )
        }
        Cmd::AssignStableLend {
            market,
            reserve,
            usdc_lamports,
            deadline_unix,
        } => {
            let market_bytes = decode_b58_pubkey(market, "market")?;
            let reserve_bytes = decode_b58_pubkey(reserve, "reserve")?;
            let assign = AssignStableLend {
                market: market_bytes,
                reserve: reserve_bytes,
                usdc_lamports: *usdc_lamports,
                deadline_unix: *deadline_unix,
            };
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&assign, &mut buf).context("serialize AssignStableLend")?;
            (
                MsgType::Assign,
                make_conversation_id(),
                buf,
                "AssignStableLend",
            )
        }
        Cmd::Approve { conv_hex } => {
            let bytes = hex::decode(conv_hex).context("decode --conv-hex")?;
            if bytes.len() != 16 {
                anyhow::bail!(
                    "--conv-hex must be 16 bytes (32 hex chars), got {}",
                    bytes.len()
                );
            }
            let mut conv = [0u8; 16];
            conv.copy_from_slice(&bytes);
            (MsgType::Approve, conv, Vec::new(), "Approve")
        }
        Cmd::AssignHedgedjlp {
            usdc_lamports,
            target_delta_bps,
            max_borrow_rate_bps,
            deadline_unix,
        } => {
            let dl = if *deadline_unix == 0 {
                now_unix() + 300
            } else {
                *deadline_unix
            };
            let assign = AssignHedgedJlp {
                usdc_lamports: *usdc_lamports,
                target_delta_bps: *target_delta_bps,
                max_borrow_rate_bps: *max_borrow_rate_bps,
                deadline_unix: dl,
            };
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&assign, &mut buf).context("serialize AssignHedgedJlp")?;
            (
                MsgType::Assign,
                make_conversation_id(),
                buf,
                "AssignHedgedJlp",
            )
        }
        Cmd::WithdrawHedgedjlp {
            jlp_lamports,
            deadline_unix,
        } => {
            let withdraw = WithdrawHedgedJlp {
                jlp_lamports: *jlp_lamports,
                deadline_unix: *deadline_unix,
            };
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&withdraw, &mut buf)
                .context("serialize WithdrawHedgedJlp")?;
            (
                MsgType::Withdraw,
                make_conversation_id(),
                buf,
                "WithdrawHedgedJlp",
            )
        }
        Cmd::WithdrawStableLend {
            market,
            reserve,
            usdc_lamports,
            deadline_unix,
        } => {
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
            (
                MsgType::Withdraw,
                make_conversation_id(),
                buf,
                "WithdrawStableLend",
            )
        }
        Cmd::WithdrawMultiply {
            vault_hex,
            max_slippage_bps,
            deadline_unix,
        } => {
            let mut vault = [0u8; 32];
            let bytes = hex::decode(vault_hex).context("decode --vault-hex")?;
            if bytes.len() != 32 {
                anyhow::bail!(
                    "--vault-hex must be 32 bytes (64 hex chars), got {}",
                    bytes.len()
                );
            }
            vault.copy_from_slice(&bytes);
            let withdraw = WithdrawMultiply {
                vault,
                max_slippage_bps: *max_slippage_bps,
                deadline_unix: *deadline_unix,
            };
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&withdraw, &mut buf)
                .context("serialize WithdrawMultiply")?;
            (
                MsgType::WithdrawMultiply,
                make_conversation_id(),
                buf,
                "WithdrawMultiply",
            )
        }
        Cmd::Allocator { .. } => {
            unreachable!("Cmd::Allocator handled by run_allocator()");
        }
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    // Allocator subcommand short-circuits the rest of main(): the
    // pure-decision codepath needs no node identity for dry-run. When
    // --execute is on, run_allocator() spins up the node itself and
    // re-uses the existing Assign/Withdraw envelope path via
    // dispatch_envelope().
    if let Cmd::Allocator { .. } = &args.cmd {
        return run_allocator(&args).await;
    }

    // Load orchestrator role identity.
    let secrets = FileSource::new(&args.secrets_dir);
    let role_id = load_role_identity(&secrets, Role::Orchestrator, "orchestrator-role.key")
        .await
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
                anyhow::bail!(
                    "--recipient-agent-id must be 32 bytes (64 hex chars), got {}",
                    bytes.len()
                );
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
    let (msg_type, conv, payload_bytes, label): (MsgType, [u8; 16], Vec<u8>, &'static str) =
        build_envelope_from_cmd(&args.cmd)?;

    info!(conv = %hex::encode(conv), label, "conv id selected");

    // Emit one BEACON so peers register our pubkey + role. Without this,
    // the bilateral request-response handler on the receiving daemon will
    // silently drop our envelope (it validates sender identity against
    // peer_states, which is populated by BEACON observations).
    if let Err(e) = send_one_beacon(&handle, &role_id).await {
        warn!(
            ?e,
            "initial BEACON send failed; recipient may drop our envelope"
        );
    }
    info!(label, "BEACON emitted");

    // When a specific recipient was given, wait until its BEACON has been
    // observed (peer registered in our peer_states map). This avoids the race
    // between gossipsub mesh formation and the first send attempt — the
    // enterprise node drops bilateral sends to unknown peer_ids silently.
    if recipient != BROADCAST_RECIPIENT {
        info!(recipient = %hex::encode(recipient), "waiting for recipient peer to register...");
        let wait_timeout = Duration::from_secs(args.timeout_secs.min(60));
        match handle.wait_for_peer(recipient, wait_timeout).await {
            Ok(()) => info!("recipient peer registered; sending"),
            Err(e) => warn!(?e, "wait_for_peer timed out — sending anyway (may fail)"),
        }
    } else {
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

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
    // Start from a time-based nonce so each pm-stub run's nonces are
    // always higher than any nonce the recipient saw in previous runs.
    let mut next_nonce = now_unix();

    // Reports that arrived with a matching conv_id but a payload type that
    // didn't match what we asked for (e.g., hedgedjlp responding to an
    // AssignStableLend). Held for the timeout-fallback log so an operator
    // can see what arrived even when the legitimate Report never did.
    let mut mismatched_reports: Vec<Envelope> = Vec::new();
    let expected_report = expected_report_for_label(label);

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
        // When --recipient-agent-id was supplied, filter Reports by sender so
        // that early Reports from unintended daemons (which broadcast-process
        // every incoming Assign) don't short-circuit the loop before the
        // intended recipient responds.
        let sender_filter: Option<[u8; 32]> = if recipient == BROADCAST_RECIPIENT {
            None
        } else {
            Some(recipient)
        };
        match tokio::time::timeout(
            wait,
            wait_for_report_loop(
                &mut handle,
                conv,
                sender_filter,
                expected_report,
                &mut mismatched_reports,
            ),
        )
        .await
        {
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
                info!(
                    attempt,
                    elapsed_secs = started.elapsed().as_secs(),
                    "no Report yet, retrying send"
                );
            }
        }
    }

    eprintln!(
        "No Report received: timed out after {:?} ({} attempts)",
        total_timeout, attempt
    );
    // Fix 3b debugging aid (2026-05-13): if any Reports arrived that we
    // filtered as wrong-type, print them so the operator can see who
    // mistakenly responded.
    if !mismatched_reports.is_empty() {
        eprintln!(
            "({} mismatched Report(s) arrived during the timeout — printing for diagnostics:)",
            mismatched_reports.len(),
        );
        for env in &mismatched_reports {
            print_report(env, "<mismatched>");
        }
    }
    std::process::exit(2);
}

/// Top-level handler for `Cmd::Allocator`. Pulls live state from the
/// dashboard REST API, runs the pure decision function, prints (and in
/// `--execute` mode also dispatches) the recommendation.
async fn run_allocator(args: &Args) -> Result<()> {
    let Cmd::Allocator {
        api_base,
        risk_premium_multiply_bps,
        risk_premium_hedgedjlp_bps,
        min_action_usd,
        max_action_fraction,
        execute,
        targets_json,
        audit_log,
    } = &args.cmd
    else {
        unreachable!("run_allocator called with non-Allocator cmd");
    };

    let cfg = allocator_runner::config_from_cli(
        *risk_premium_multiply_bps,
        *risk_premium_hedgedjlp_bps,
        *min_action_usd,
        *max_action_fraction,
    );

    info!(api_base, "fetching fleet snapshot");
    let snap = allocator_runner::fetch_snapshot(api_base)
        .await
        .context("fetch fleet snapshot")?;

    let action = allocator::decide(&snap.strategies, snap.total_aum_usd, snap.idle_usd, &cfg);
    allocator_runner::print_action(&action, &snap);

    if !*execute {
        // Dry-run: optionally append an audit record with mode=dry-run so
        // operators have a single chronological JSONL to grep.
        let rec = allocator_runner::AuditRecord {
            ts_unix: allocator_runner::now_unix(),
            mode: "dry-run",
            snapshot: allocator_runner::AuditSnapshot::from(&snap),
            action: &action,
            envelope_result: String::new(),
        };
        if let Err(e) = allocator_runner::append_audit(audit_log, &rec) {
            warn!(?e, "could not append dry-run audit record (continuing)");
        }
        return Ok(());
    }

    // --- --execute path ---
    // 1. Load targets json.
    let targets_path = targets_json
        .as_ref()
        .context("--execute requires --targets-json <path>")?;
    let targets =
        allocator_runner::ExecuteTargets::load(targets_path).context("load --targets-json")?;

    // 2. Translate the allocator action into a wire-ready envelope spec
    //    via the shared library function. The orchestrator-daemon (v0.4.0)
    //    will go through this exact function — keeping one source of truth
    //    for envelope construction between the CLI and the daemon.
    let spec = match allocator_runner::action_to_envelope_spec(&action, &targets)? {
        Some(s) => s,
        None => {
            let rec = allocator_runner::AuditRecord {
                ts_unix: allocator_runner::now_unix(),
                mode: "execute",
                snapshot: allocator_runner::AuditSnapshot::from(&snap),
                action: &action,
                envelope_result: "skipped:no_dispatch".to_string(),
            };
            if let Err(e) = allocator_runner::append_audit(audit_log, &rec) {
                warn!(?e, "could not append audit record");
            }
            info!("nothing to dispatch (NoAction or unsupported action shape)");
            return Ok(());
        }
    };
    info!(
        label = spec.label,
        conv = %hex::encode(spec.conv_id),
        "allocator dispatch envelope built",
    );

    // 3. Spin up the same node + envelope path the other subcommands use.
    let secrets = FileSource::new(&args.secrets_dir);
    let role_id = load_role_identity(&secrets, Role::Orchestrator, "orchestrator-role.key")
        .await
        .context("load orchestrator role identity")?;
    let node_cfg = build_node_config(args, &role_id)?;
    let service = NodeService::build(node_cfg).await?;
    let handle = service.handle();
    let _node_task = tokio::spawn(service.run());
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 4. Pull recipient + envelope ingredients out of the spec.
    let recipient = spec.recipient;
    let msg_type = spec.msg_type;
    let conv = spec.conv_id;
    let payload_bytes = spec.payload;
    let label = spec.label;

    // 6. BEACON + wait_for_peer + retry-send loop, mirroring the standard
    //    path. We bound the total wait to args.timeout_secs.
    if let Err(e) = send_one_beacon(&handle, &role_id).await {
        warn!(?e, "initial BEACON send failed");
    }
    let wait_timeout = Duration::from_secs(args.timeout_secs.min(60));
    if let Err(e) = handle.wait_for_peer(recipient, wait_timeout).await {
        warn!(?e, "wait_for_peer timed out — sending anyway");
    }

    let signing_key = ed25519_dalek::SigningKey::from_bytes(role_id.signing_key_bytes());
    let sender_pubkey = signing_key.verifying_key().to_bytes();
    let mut next_nonce = now_unix();
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
    let _ = next_nonce; // suppress unused-warning when path is single-shot.

    let envelope_result = match handle.send(env).await {
        Ok(()) => {
            info!(label, "allocator envelope sent");
            "sent".to_string()
        }
        Err(e) => {
            warn!(?e, "allocator send failed");
            format!("failed:{e}")
        }
    };

    // 7. Audit log. We don't wait on the Report — the allocator is a
    //    periodic process; if the Report arrives later it's picked up by
    //    the dashboard's normal ingest. Operators see the action +
    //    envelope outcome here.
    let rec = allocator_runner::AuditRecord {
        ts_unix: allocator_runner::now_unix(),
        mode: "execute",
        snapshot: allocator_runner::AuditSnapshot::from(&snap),
        action: &action,
        envelope_result,
    };
    if let Err(e) = allocator_runner::append_audit(audit_log, &rec) {
        warn!(?e, "could not append audit record");
    }

    Ok(())
}


#[cfg(test)]
mod withdraw_multiply_envelope_tests {
    //! Pin the `withdraw-multiply` subcommand's envelope shape so a future
    //! refactor doesn't silently route to the wrong MsgType or label.
    use super::*;

    #[test]
    fn withdraw_multiply_produces_correct_envelope() {
        let cmd = Cmd::WithdrawMultiply {
            vault_hex: "01".repeat(32),
            max_slippage_bps: 100,
            deadline_unix: 0,
        };
        let (msg_type, _conv, payload, label) =
            build_envelope_from_cmd(&cmd).expect("build envelope");
        assert_eq!(msg_type, MsgType::WithdrawMultiply);
        assert_eq!(label, "WithdrawMultiply");
        // Payload must CBOR-decode back to WithdrawMultiply.
        let decoded: WithdrawMultiply =
            ciborium::de::from_reader(&payload[..]).expect("decode WithdrawMultiply");
        assert_eq!(decoded.max_slippage_bps, 100);
        assert_eq!(decoded.deadline_unix, 0);
        assert_eq!(decoded.vault, [1u8; 32]);
    }

    #[test]
    fn withdraw_multiply_rejects_short_vault_hex() {
        let cmd = Cmd::WithdrawMultiply {
            // 31 bytes — must reject.
            vault_hex: "01".repeat(31),
            max_slippage_bps: 100,
            deadline_unix: 0,
        };
        let err = build_envelope_from_cmd(&cmd).unwrap_err();
        assert!(err.to_string().contains("32 bytes"));
    }
}
