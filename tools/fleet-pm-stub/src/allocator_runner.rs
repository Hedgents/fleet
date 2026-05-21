//! HTTP plumbing + envelope-sending glue for the regime-aware allocator.
//!
//! `allocator.rs` is pure and deterministic. This module is the I/O
//! sidecar: it pulls live state from the dashboard's REST API
//! (`/strategies`, `/aum`, `/rates`), wraps it into `StrategyRate`/idle
//! values for `decide()`, and (when `--execute`) wires the recommended
//! action into the existing Assign/Withdraw envelope path that
//! `fleet-pm-stub` already uses for the other subcommands.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use zerox1_protocol::fleet::hedgedjlp::{AssignHedgedJlp, WithdrawHedgedJlp};
use zerox1_protocol::fleet::multiply::AssignMultiply;
use zerox1_protocol::fleet::stable_lend::{AssignStableLend, WithdrawStableLend};
use zerox1_protocol::message::MsgType;

use crate::allocator::{AllocatorAction, AllocatorConfig, StrategyRate};

/// Single strategy card from `GET /strategies`. Only the fields the
/// allocator cares about are pulled — extra fields are ignored.
#[derive(Debug, Deserialize)]
struct StrategyCard {
    id: String,
    #[serde(default)]
    deployed_usdc: f64,
    /// Live APR in basis points (unsigned u32 in dashboard; we widen to
    /// i32 because the allocator models signed APR for negative carry).
    #[serde(default)]
    current_apr_bps: u32,
}

#[derive(Debug, Deserialize)]
struct StrategiesResponse {
    strategies: Vec<StrategyCard>,
}

#[derive(Debug, Deserialize)]
struct AumPerStrategy {
    #[serde(default)]
    idle_usdc: f64,
}

#[derive(Debug, Deserialize)]
struct AumResponse {
    #[serde(default)]
    total_usdc: f64,
    per_strategy: AumPerStrategy,
}

/// Live snapshot the allocator consumed. Returned to callers so the
/// dry-run print + audit log show the exact inputs used.
#[derive(Debug, Clone)]
pub struct FleetSnapshot {
    pub strategies: Vec<StrategyRate>,
    pub total_aum_usd: f64,
    pub idle_usd: f64,
}

/// Fetch `/strategies` + `/aum` from the dashboard REST API and assemble
/// a `FleetSnapshot`. The dashboard's `/rates` is implicitly already
/// baked into `current_apr_bps` for `stable_yield`, so no separate call
/// is required.
pub async fn fetch_snapshot(api_base: &str) -> Result<FleetSnapshot> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .context("build reqwest client")?;

    let strategies_url = format!("{}/strategies", api_base.trim_end_matches('/'));
    let aum_url = format!("{}/aum", api_base.trim_end_matches('/'));

    let strategies: StrategiesResponse = client
        .get(&strategies_url)
        .send()
        .await
        .with_context(|| format!("GET {strategies_url}"))?
        .error_for_status()?
        .json()
        .await
        .context("decode /strategies json")?;
    let aum: AumResponse = client
        .get(&aum_url)
        .send()
        .await
        .with_context(|| format!("GET {aum_url}"))?
        .error_for_status()?
        .json()
        .await
        .context("decode /aum json")?;

    let strategies: Vec<StrategyRate> = strategies
        .strategies
        .into_iter()
        .map(|c| StrategyRate {
            id: c.id,
            deployed_usd: c.deployed_usdc,
            nominal_apr_bps: c.current_apr_bps as i32,
        })
        .collect();

    Ok(FleetSnapshot {
        strategies,
        total_aum_usd: aum.total_usdc,
        idle_usd: aum.per_strategy.idle_usdc,
    })
}

/// One audit log record per allocator tick. Written as JSONL to the
/// configured audit-log path when `--execute` is on (and harmlessly
/// constructible in dry-run for future plumbing).
#[derive(Debug, Serialize)]
pub struct AuditRecord<'a> {
    pub ts_unix: u64,
    pub mode: &'a str, // "dry-run" | "execute"
    pub snapshot: AuditSnapshot,
    pub action: &'a AllocatorAction,
    /// Empty in dry-run. In execute mode: "sent", "skipped:<reason>",
    /// or "failed:<error>". Per-recipient envelope outcome.
    pub envelope_result: String,
}

#[derive(Debug, Serialize)]
pub struct AuditSnapshot {
    pub total_aum_usd: f64,
    pub idle_usd: f64,
    pub strategies: Vec<AuditStrategy>,
}

#[derive(Debug, Serialize)]
pub struct AuditStrategy {
    pub id: String,
    pub deployed_usd: f64,
    pub nominal_apr_bps: i32,
    /// Strategy's share of total AUM (`deployed_usd / total_aum_usd`).
    /// `Some` whenever total_aum_usd > 0; `None` on degenerate snapshots.
    /// Useful even without targets: operators can read the live
    /// allocation from a single audit row without recomputing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_weight: Option<f64>,
    /// Operator's target share from [`TargetWeights`]. Populated only
    /// in drift mode (allocator v2 M2+). Omitted on the wire in greedy
    /// mode to keep the JSONL backwards-compatible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_weight: Option<f64>,
    /// `(current_weight - target_weight) × 10_000`. Positive =
    /// overweight. Populated only in drift mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift_bps: Option<i32>,
}

impl AuditSnapshot {
    /// Backwards-compatible constructor. Equivalent to
    /// `from_with_targets(snap, None)`. Existing callers (orchestrator
    /// daemon tick, fleet-pm-stub CLI subcommands) keep working unchanged
    /// — they get `current_weight` populated automatically (cheap and
    /// useful) but `target_weight` / `drift_bps` omitted, exactly as
    /// before M3.
    pub fn from(snap: &FleetSnapshot) -> Self {
        Self::from_with_targets(snap, None)
    }

    /// Audit-record constructor that captures the full drift-mode state
    /// when `targets` is supplied. M3 keeps this opt-in; the orchestrator
    /// (M4) will pass `Some(&targets)` when running in drift mode so
    /// every JSONL row records the exact (current, target, drift) tuple
    /// the picker saw.
    pub fn from_with_targets(
        snap: &FleetSnapshot,
        targets: Option<&crate::allocator_targets::TargetWeights>,
    ) -> Self {
        let total = snap.total_aum_usd;
        Self {
            total_aum_usd: total,
            idle_usd: snap.idle_usd,
            strategies: snap
                .strategies
                .iter()
                .map(|s| {
                    let (current_weight, target_weight, drift_bps) = if total > 0.0 {
                        let cw = s.deployed_usd / total;
                        match targets {
                            Some(t) => {
                                let tw = t.for_strategy(&s.id);
                                let drift = ((cw - tw) * 10_000.0).round() as i32;
                                (Some(cw), Some(tw), Some(drift))
                            }
                            None => (Some(cw), None, None),
                        }
                    } else {
                        (None, None, None)
                    };
                    AuditStrategy {
                        id: s.id.clone(),
                        deployed_usd: s.deployed_usd,
                        nominal_apr_bps: s.nominal_apr_bps,
                        current_weight,
                        target_weight,
                        drift_bps,
                    }
                })
                .collect(),
        }
    }
}

/// Targets config (loaded from JSON) telling the executor which agent_id
/// owns each strategy daemon plus the Kamino market/reserve pubkeys
/// needed for AssignStableLend / WithdrawStableLend envelopes.
///
/// Example file (when `--execute` is on):
/// ```json
/// {
///   "stable_yield": {
///     "recipient_agent_id_hex": "abcd...",
///     "market_b58": "Hub5...",
///     "reserve_b58": "9Tv9..."
///   },
///   "multiply": { "recipient_agent_id_hex": "1234..." },
///   "hedgedjlp": { "recipient_agent_id_hex": "5678..." }
/// }
/// ```
#[derive(Debug, Deserialize)]
pub struct ExecuteTargets {
    #[serde(default)]
    pub stable_yield: Option<StableLendTarget>,
    #[serde(default)]
    pub multiply: Option<RecipientTarget>,
    #[serde(default)]
    pub hedgedjlp: Option<RecipientTarget>,
}

#[derive(Debug, Deserialize)]
pub struct RecipientTarget {
    pub recipient_agent_id_hex: String,
}

#[derive(Debug, Deserialize)]
pub struct StableLendTarget {
    pub recipient_agent_id_hex: String,
    pub market_b58: String,
    pub reserve_b58: String,
}

impl ExecuteTargets {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("read targets json at {}", path.display()))?;
        let parsed: ExecuteTargets =
            serde_json::from_slice(&bytes).context("parse targets json")?;
        Ok(parsed)
    }
}

/// Helper: append one JSONL line to the audit log file. Creates parent
/// dirs and the file as needed. Used by the `--execute` codepath.
pub fn append_audit(audit_path: &Path, record: &AuditRecord<'_>) -> Result<()> {
    if let Some(parent) = audit_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut line = serde_json::to_string(record).context("serialize audit record")?;
    line.push('\n');
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path)
        .with_context(|| format!("open audit log {}", audit_path.display()))?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// Current unix seconds (test-friendly wrapper).
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pretty-print one allocator action to stdout (used by dry-run + by
/// execute mode's pre-flight log).
pub fn print_action(action: &AllocatorAction, snap: &FleetSnapshot) {
    println!("── allocator snapshot ─────────────────────────────────");
    println!(
        "  total_aum_usd = {:.4}    idle_usd = {:.4}",
        snap.total_aum_usd, snap.idle_usd
    );
    for s in &snap.strategies {
        println!(
            "  {:<14} deployed=${:>10.4}   apr={:>7.2}%",
            s.id,
            s.deployed_usd,
            (s.nominal_apr_bps as f64) / 100.0
        );
    }
    println!("── recommendation ─────────────────────────────────────");
    match action {
        AllocatorAction::NoAction { reason } => {
            println!("  NoAction: {reason}");
        }
        AllocatorAction::Withdraw {
            strategy,
            amount_usd,
            reason,
        } => {
            println!("  Withdraw {strategy} ${amount_usd:.4}");
            println!("    reason: {reason}");
        }
        AllocatorAction::Deposit {
            strategy,
            amount_usd,
            reason,
        } => {
            println!("  Deposit {strategy} ${amount_usd:.4}");
            println!("    reason: {reason}");
        }
    }
    println!("───────────────────────────────────────────────────────");
}

/// Decode the `AllocatorConfig` from raw CLI bps + USD values. Wraps the
/// struct construction so callers don't need to know the field names.
/// Callers that don't want to plumb `min_withdraw_gap_bps` through the
/// CLI inherit the `AllocatorConfig::default()` value (150 bps).
pub fn config_from_cli(
    risk_premium_multiply_bps: i32,
    risk_premium_hedgedjlp_bps: i32,
    min_action_usd: f64,
    max_action_fraction: f64,
) -> AllocatorConfig {
    AllocatorConfig {
        risk_premium_bps_multiply: risk_premium_multiply_bps,
        risk_premium_bps_hedgedjlp: risk_premium_hedgedjlp_bps,
        min_action_usd,
        max_action_fraction,
        ..AllocatorConfig::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allocator::AllocatorAction;

    #[test]
    fn audit_record_roundtrips_to_jsonl() {
        let snap = FleetSnapshot {
            strategies: vec![StrategyRate {
                id: "stable_yield".to_string(),
                deployed_usd: 100.0,
                nominal_apr_bps: 701,
            }],
            total_aum_usd: 100.0,
            idle_usd: 0.0,
        };
        let action = AllocatorAction::NoAction {
            reason: "ok".to_string(),
        };
        let rec = AuditRecord {
            ts_unix: 1700000000,
            mode: "dry-run",
            snapshot: AuditSnapshot::from(&snap),
            action: &action,
            envelope_result: String::new(),
        };
        let dir = std::env::temp_dir();
        let path = dir.join(format!("allocator-audit-test-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        append_audit(&path, &rec).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(body.ends_with('\n'));
        let parsed: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(parsed["mode"], "dry-run");
        assert_eq!(parsed["action"]["action"], "no_action");
        assert_eq!(parsed["snapshot"]["strategies"][0]["id"], "stable_yield");
    }

    #[test]
    fn config_from_cli_round_trip() {
        let c = config_from_cli(150, 250, 10.0, 0.25);
        assert_eq!(c.risk_premium_bps_multiply, 150);
        assert_eq!(c.risk_premium_bps_hedgedjlp, 250);
        assert!((c.min_action_usd - 10.0).abs() < 1e-9);
        assert!((c.max_action_fraction - 0.25).abs() < 1e-9);
    }

    fn three_strat_snap() -> FleetSnapshot {
        FleetSnapshot {
            strategies: vec![
                StrategyRate {
                    id: "stable_yield".to_string(),
                    deployed_usd: 30.0,
                    nominal_apr_bps: 500,
                },
                StrategyRate {
                    id: "multiply".to_string(),
                    deployed_usd: 30.0,
                    nominal_apr_bps: 1500,
                },
                StrategyRate {
                    id: "hedgedjlp".to_string(),
                    deployed_usd: 40.0,
                    nominal_apr_bps: 1500,
                },
            ],
            total_aum_usd: 100.0,
            idle_usd: 0.0,
        }
    }

    #[test]
    fn audit_snapshot_greedy_mode_emits_current_weight_no_targets() {
        // Greedy mode (target_weights: None). Each strategy row gets
        // `current_weight` because it's free to compute and useful, but
        // `target_weight` and `drift_bps` MUST be omitted from the JSON
        // — those fields are drift-mode-specific and operators reading
        // greedy-mode rows shouldn't see them at all.
        let snap = three_strat_snap();
        let audit = AuditSnapshot::from(&snap);
        let v = serde_json::to_value(&audit).expect("serialize");
        let arr = v["strategies"].as_array().expect("strategies array");
        for row in arr {
            assert!(
                row.get("current_weight").is_some(),
                "current_weight should always be populated when total_aum>0: {row}"
            );
            assert!(
                row.get("target_weight").is_none(),
                "target_weight must be omitted in greedy mode: {row}"
            );
            assert!(
                row.get("drift_bps").is_none(),
                "drift_bps must be omitted in greedy mode: {row}"
            );
        }
        // Spot-check the actual weight: stable_yield $30 of $100 = 0.30.
        let stable = &arr[0];
        assert_eq!(stable["id"], "stable_yield");
        let cw = stable["current_weight"].as_f64().unwrap();
        assert!((cw - 0.30).abs() < 1e-9);
    }

    #[test]
    fn audit_snapshot_drift_mode_emits_full_weight_triple() {
        // Drift mode (target_weights: Some). Every row must carry
        // current_weight, target_weight, AND drift_bps so the JSONL is
        // a complete forensic record of the picker's input.
        let snap = three_strat_snap();
        let targets =
            crate::allocator_targets::TargetWeights::new(0.30, 0.30, 0.40).unwrap();
        let audit = AuditSnapshot::from_with_targets(&snap, Some(&targets));
        let v = serde_json::to_value(&audit).expect("serialize");
        let arr = v["strategies"].as_array().expect("strategies array");

        // stable_yield: current=0.30, target=0.30 → drift=0.
        let stable = arr
            .iter()
            .find(|r| r["id"] == "stable_yield")
            .expect("stable row");
        assert!((stable["current_weight"].as_f64().unwrap() - 0.30).abs() < 1e-9);
        assert!((stable["target_weight"].as_f64().unwrap() - 0.30).abs() < 1e-9);
        assert_eq!(stable["drift_bps"].as_i64().unwrap(), 0);

        // hedgedjlp: current=0.40, target=0.40 → drift=0.
        let hedge = arr
            .iter()
            .find(|r| r["id"] == "hedgedjlp")
            .expect("hedge row");
        assert!((hedge["current_weight"].as_f64().unwrap() - 0.40).abs() < 1e-9);
        assert!((hedge["target_weight"].as_f64().unwrap() - 0.40).abs() < 1e-9);
        assert_eq!(hedge["drift_bps"].as_i64().unwrap(), 0);
    }

    #[test]
    fn audit_snapshot_drift_mode_captures_actual_drift_in_bps() {
        // Tilt the snapshot so the drifts are non-zero and asymmetric.
        // current: stable=0.50, multiply=0.20, hedgedjlp=0.30
        // target:  stable=0.30, multiply=0.30, hedgedjlp=0.40
        // drift:   stable=+2000, multiply=-1000, hedgedjlp=-1000 bps
        let snap = FleetSnapshot {
            strategies: vec![
                StrategyRate {
                    id: "stable_yield".to_string(),
                    deployed_usd: 50.0,
                    nominal_apr_bps: 500,
                },
                StrategyRate {
                    id: "multiply".to_string(),
                    deployed_usd: 20.0,
                    nominal_apr_bps: 1500,
                },
                StrategyRate {
                    id: "hedgedjlp".to_string(),
                    deployed_usd: 30.0,
                    nominal_apr_bps: 1500,
                },
            ],
            total_aum_usd: 100.0,
            idle_usd: 0.0,
        };
        let targets =
            crate::allocator_targets::TargetWeights::new(0.30, 0.30, 0.40).unwrap();
        let audit = AuditSnapshot::from_with_targets(&snap, Some(&targets));
        let v = serde_json::to_value(&audit).unwrap();
        let arr = v["strategies"].as_array().unwrap();
        let get_drift = |id: &str| -> i64 {
            arr.iter()
                .find(|r| r["id"] == id)
                .unwrap()["drift_bps"]
                .as_i64()
                .unwrap()
        };
        assert_eq!(get_drift("stable_yield"), 2000);
        assert_eq!(get_drift("multiply"), -1000);
        assert_eq!(get_drift("hedgedjlp"), -1000);
    }

    #[test]
    fn audit_snapshot_zero_aum_omits_all_weight_fields() {
        // Degenerate snapshot: total_aum=0. Current_weight would divide
        // by zero, so we must omit all three weight fields rather than
        // emit NaN or sentinel values.
        let snap = FleetSnapshot {
            strategies: vec![StrategyRate {
                id: "stable_yield".to_string(),
                deployed_usd: 0.0,
                nominal_apr_bps: 500,
            }],
            total_aum_usd: 0.0,
            idle_usd: 0.0,
        };
        let targets =
            crate::allocator_targets::TargetWeights::new(1.0, 0.0, 0.0).unwrap();
        let audit = AuditSnapshot::from_with_targets(&snap, Some(&targets));
        let v = serde_json::to_value(&audit).unwrap();
        let row = &v["strategies"][0];
        assert!(row.get("current_weight").is_none(), "got: {row}");
        assert!(row.get("target_weight").is_none(), "got: {row}");
        assert!(row.get("drift_bps").is_none(), "got: {row}");
        // But the basic fields (id, deployed_usd, nominal_apr_bps) must
        // still serialise — the row remains useful for the audit log
        // even when weight math doesn't apply.
        assert_eq!(row["id"], "stable_yield");
    }
}

// ===========================================================================
// EnvelopeSpec + action_to_envelope_spec
//
// The pure decision function lives in `allocator.rs`. The HTTP-fetch glue
// lives above. This section converts an `AllocatorAction` into the
// wire-ready envelope ingredients (msg_type, recipient, conv_id, payload,
// label) so any caller — the CLI's `run_allocator()` or the long-running
// orchestrator-daemon — can build, sign, and send the envelope from the
// same source of truth.
//
// Both callers go through this function. If we ever diverge — e.g. CLI
// emitting a stale Assign shape after the daemon was updated — the unit
// tests below will catch it before mainnet does.
// ===========================================================================

/// Wire-ready envelope ingredients. Caller is responsible for building
/// the signed `Envelope` from this + a sender pubkey + a nonce.
#[derive(Debug, Clone)]
pub struct EnvelopeSpec {
    pub msg_type: MsgType,
    pub recipient: [u8; 32],
    pub conv_id: [u8; 16],
    pub payload: Vec<u8>,
    /// Human-readable label for logs + audit (`"AssignStableLend"` etc).
    pub label: &'static str,
}

/// Convert an allocator decision into a wire-ready envelope spec, looking
/// up the recipient agent_id and any per-strategy parameters
/// (market/reserve for Kamino stable-lend) from `targets`.
///
/// Returns `Ok(None)` for [`AllocatorAction::NoAction`] and for action
/// shapes we deliberately do not dispatch — currently
/// `Deposit{multiply}` (multiply has no USD-sizing parameter in its
/// Assign envelope; depositing requires an out-of-band wallet transfer
/// first).
///
/// Withdraw `multiply` is implemented as `AssignMultiply{target_ltv_bps=0}`
/// — the multiply daemon interprets that as full deleverage on its next
/// cycle, matching the CLI's existing behaviour. v0.4.x will switch this
/// to `WithdrawMultiply` once the iterative unwind is the default path.
pub fn action_to_envelope_spec(
    action: &AllocatorAction,
    targets: &ExecuteTargets,
) -> Result<Option<EnvelopeSpec>> {
    Ok(match action {
        AllocatorAction::NoAction { .. } => None,

        AllocatorAction::Withdraw {
            strategy,
            amount_usd,
            ..
        } => match strategy.as_str() {
            "stable_yield" => {
                let t = targets
                    .stable_yield
                    .as_ref()
                    .context("targets.stable_yield missing for Withdraw{stable_yield}")?;
                let recipient = decode_recipient_hex(&t.recipient_agent_id_hex)?;
                let market = decode_b58_pubkey(&t.market_b58, "market")?;
                let reserve = decode_b58_pubkey(&t.reserve_b58, "reserve")?;
                let payload = WithdrawStableLend {
                    market,
                    reserve,
                    usdc_lamports: usd_to_usdc_lamports(*amount_usd),
                    deadline_unix: 0,
                };
                Some(EnvelopeSpec {
                    msg_type: MsgType::Withdraw,
                    recipient,
                    conv_id: make_conversation_id(),
                    payload: cbor(&payload, "WithdrawStableLend")?,
                    label: "WithdrawStableLend",
                })
            }
            "hedgedjlp" => {
                let t = targets
                    .hedgedjlp
                    .as_ref()
                    .context("targets.hedgedjlp missing for Withdraw{hedgedjlp}")?;
                let recipient = decode_recipient_hex(&t.recipient_agent_id_hex)?;
                // hedgedjlp Withdraw is intentionally all-or-nothing.
                //
                // The hedgedjlp daemon's `unwind.rs` iterates
                // `active.open_positions` and closes every short
                // unconditionally; there is no proportional-close path
                // today. If the allocator emitted a partial
                // `jlp_lamports`, the daemon would burn N% of the JLP
                // but still close 100% of the shorts, leaving the
                // residual JLP unhedged — strictly worse than fully
                // unwinding.
                //
                // The right pairing for this constraint is the
                // `min_withdraw_gap_bps` hysteresis in `allocator.rs`
                // (default 150 bps): Withdraw only fires when carry is
                // genuinely inverted by a meaningful margin, never on
                // a single-tick APR-spike. The two together encode:
                // "the only Withdraw we ever emit is a full liquidation,
                // and we only emit it when the operator would have
                // wanted one anyway."
                //
                // When proportional unwind lands in the hedgedjlp
                // daemon, this `u64::MAX` will be replaced with a
                // proportional `jlp_lamports` derived from
                // `amount_usd / hedgedjlp.deployed_usd * jlp_lamports_total`.
                let payload = WithdrawHedgedJlp {
                    jlp_lamports: u64::MAX,
                    deadline_unix: 0,
                };
                Some(EnvelopeSpec {
                    msg_type: MsgType::Withdraw,
                    recipient,
                    conv_id: make_conversation_id(),
                    payload: cbor(&payload, "WithdrawHedgedJlp")?,
                    label: "WithdrawHedgedJlp",
                })
            }
            "multiply" => {
                let t = targets
                    .multiply
                    .as_ref()
                    .context("targets.multiply missing for Withdraw{multiply}")?;
                let recipient = decode_recipient_hex(&t.recipient_agent_id_hex)?;
                // Re-Assign target_ltv_bps=0 = full deleverage. The
                // multiply daemon's existing handler covers this path.
                let payload = AssignMultiply {
                    vault: [0u8; 32],
                    target_ltv_bps: 0,
                    max_slippage_bps: 50,
                    deadline_unix: now_unix() + 300,
                };
                Some(EnvelopeSpec {
                    msg_type: MsgType::Assign,
                    recipient,
                    conv_id: make_conversation_id(),
                    payload: cbor(&payload, "AssignMultiply(deleverage)")?,
                    label: "AssignMultiply",
                })
            }
            other => anyhow::bail!("Withdraw target strategy '{other}' is unknown"),
        },

        AllocatorAction::Deposit {
            strategy,
            amount_usd,
            ..
        } => match strategy.as_str() {
            "stable_yield" => {
                let t = targets
                    .stable_yield
                    .as_ref()
                    .context("targets.stable_yield missing for Deposit{stable_yield}")?;
                let recipient = decode_recipient_hex(&t.recipient_agent_id_hex)?;
                let market = decode_b58_pubkey(&t.market_b58, "market")?;
                let reserve = decode_b58_pubkey(&t.reserve_b58, "reserve")?;
                let payload = AssignStableLend {
                    market,
                    reserve,
                    usdc_lamports: usd_to_usdc_lamports(*amount_usd),
                    deadline_unix: 0,
                };
                Some(EnvelopeSpec {
                    msg_type: MsgType::Assign,
                    recipient,
                    conv_id: make_conversation_id(),
                    payload: cbor(&payload, "AssignStableLend")?,
                    label: "AssignStableLend",
                })
            }
            "hedgedjlp" => {
                let t = targets
                    .hedgedjlp
                    .as_ref()
                    .context("targets.hedgedjlp missing for Deposit{hedgedjlp}")?;
                let recipient = decode_recipient_hex(&t.recipient_agent_id_hex)?;
                let payload = AssignHedgedJlp {
                    usdc_lamports: usd_to_usdc_lamports(*amount_usd),
                    target_delta_bps: 0,
                    max_borrow_rate_bps: 5_000,
                    deadline_unix: 0,
                };
                Some(EnvelopeSpec {
                    msg_type: MsgType::Assign,
                    recipient,
                    conv_id: make_conversation_id(),
                    payload: cbor(&payload, "AssignHedgedJlp")?,
                    label: "AssignHedgedJlp",
                })
            }
            "multiply" => {
                // multiply's AssignMultiply has no USD-sizing field; the
                // daemon trades against whatever balance it already
                // holds. Allocator-driven deposits require an out-of-band
                // wallet transfer first.
                //
                // This branch should be unreachable in normal operation
                // — `allocator::is_deployable_via_allocator("multiply")`
                // returns `false`, so the deposit-picker skips multiply
                // and falls through to the next-best target or
                // stable_yield. We keep the `None` arm as a defence in
                // depth: if a future config relaxes the filter, the
                // dispatcher still skips cleanly with
                // `skipped:no_dispatch` rather than panicking.
                None
            }
            other => anyhow::bail!("Deposit target strategy '{other}' is unknown"),
        },
    })
}

fn cbor<T: serde::Serialize>(payload: &T, label: &'static str) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    ciborium::ser::into_writer(payload, &mut buf)
        .with_context(|| format!("serialize {label}"))?;
    Ok(buf)
}

fn decode_recipient_hex(s: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(s).context("decode recipient_agent_id_hex")?;
    if bytes.len() != 32 {
        anyhow::bail!(
            "recipient_agent_id_hex must decode to 32 bytes (got {})",
            bytes.len()
        );
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn decode_b58_pubkey(s: &str, label: &str) -> Result<[u8; 32]> {
    let bytes = bs58::decode(s)
        .into_vec()
        .with_context(|| format!("decode {label} as base58"))?;
    if bytes.len() != 32 {
        anyhow::bail!("{label} must decode to 32 bytes (got {})", bytes.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn make_conversation_id() -> [u8; 16] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    nanos.to_be_bytes()
}

fn usd_to_usdc_lamports(usd: f64) -> u64 {
    let clamped = usd.max(0.0);
    (clamped * 1_000_000.0).round() as u64
}

#[cfg(test)]
mod envelope_spec_tests {
    use super::*;
    use crate::allocator::AllocatorAction;

    fn targets() -> ExecuteTargets {
        ExecuteTargets {
            stable_yield: Some(StableLendTarget {
                recipient_agent_id_hex: "aa".repeat(32),
                market_b58: "HubrvD2pCNvVPVnSAR5Y8j8GsBxnxn3VTpdT9KbW18bM".to_string(),
                reserve_b58: "9TD2TSv4pENb8VwfbVYg25jvym7HN6iuAR6pFNSrKjqQ".to_string(),
            }),
            multiply: Some(RecipientTarget {
                recipient_agent_id_hex: "bb".repeat(32),
            }),
            hedgedjlp: Some(RecipientTarget {
                recipient_agent_id_hex: "cc".repeat(32),
            }),
        }
    }

    #[test]
    fn no_action_returns_none() {
        let a = AllocatorAction::NoAction {
            reason: "n/a".into(),
        };
        assert!(action_to_envelope_spec(&a, &targets()).unwrap().is_none());
    }

    #[test]
    fn deposit_stable_yield_emits_assign_stable_lend() {
        let a = AllocatorAction::Deposit {
            strategy: "stable_yield".into(),
            amount_usd: 50.0,
            reason: "test".into(),
        };
        let spec = action_to_envelope_spec(&a, &targets())
            .unwrap()
            .expect("expected Some");
        assert_eq!(spec.label, "AssignStableLend");
        assert_eq!(spec.msg_type, MsgType::Assign);
        assert_eq!(spec.recipient, [0xaa; 32]);
        assert!(!spec.payload.is_empty());
    }

    #[test]
    fn withdraw_stable_yield_emits_withdraw_stable_lend() {
        let a = AllocatorAction::Withdraw {
            strategy: "stable_yield".into(),
            amount_usd: 25.0,
            reason: "test".into(),
        };
        let spec = action_to_envelope_spec(&a, &targets())
            .unwrap()
            .expect("expected Some");
        assert_eq!(spec.label, "WithdrawStableLend");
        assert_eq!(spec.msg_type, MsgType::Withdraw);
    }

    #[test]
    fn withdraw_multiply_emits_assign_multiply_deleverage() {
        let a = AllocatorAction::Withdraw {
            strategy: "multiply".into(),
            amount_usd: 100.0,
            reason: "test".into(),
        };
        let spec = action_to_envelope_spec(&a, &targets())
            .unwrap()
            .expect("expected Some");
        // Deleverage path: AssignMultiply (Assign msg type) with target_ltv_bps=0.
        assert_eq!(spec.label, "AssignMultiply");
        assert_eq!(spec.msg_type, MsgType::Assign);
        assert_eq!(spec.recipient, [0xbb; 32]);
    }

    #[test]
    fn deposit_multiply_returns_none() {
        let a = AllocatorAction::Deposit {
            strategy: "multiply".into(),
            amount_usd: 100.0,
            reason: "test".into(),
        };
        // multiply has no USD-sizing field — out-of-band wallet transfer
        // required. The allocator must not pretend it can dispatch this.
        assert!(action_to_envelope_spec(&a, &targets()).unwrap().is_none());
    }

    #[test]
    fn withdraw_hedgedjlp_always_full_close_regardless_of_amount() {
        // Invariant: until the hedgedjlp daemon supports proportional
        // unwind, every WithdrawHedgedJlp envelope from the allocator
        // MUST carry `jlp_lamports: u64::MAX`. The allocator's
        // `min_withdraw_gap_bps` hysteresis exists precisely because of
        // this constraint — a small `amount_usd` from a noisy hurdle
        // overshoot must NOT translate into "burn 10% of JLP but close
        // 100% of hedges" (which would leave the residual unhedged).
        //
        // If this test ever fails because the envelope now respects
        // `amount_usd`, the daemon-side proportional-close must have
        // landed — at which point you can relax the hysteresis and add
        // a partial-withdraw integration test.
        let allocator_actions = [
            AllocatorAction::Withdraw {
                strategy: "hedgedjlp".into(),
                amount_usd: 5.0, // tiny dust
                reason: "test".into(),
            },
            AllocatorAction::Withdraw {
                strategy: "hedgedjlp".into(),
                amount_usd: 50.0, // partial
                reason: "test".into(),
            },
            AllocatorAction::Withdraw {
                strategy: "hedgedjlp".into(),
                amount_usd: 5_000.0, // larger than any realistic position
                reason: "test".into(),
            },
        ];
        for a in &allocator_actions {
            let spec = action_to_envelope_spec(a, &targets())
                .unwrap()
                .expect("expected Some");
            assert_eq!(spec.label, "WithdrawHedgedJlp");
            assert_eq!(spec.msg_type, MsgType::Withdraw);
            assert_eq!(spec.recipient, [0xcc; 32]);
            // Decode the CBOR payload and inspect jlp_lamports.
            let decoded: zerox1_protocol::fleet::hedgedjlp::WithdrawHedgedJlp =
                ciborium::de::from_reader(&spec.payload[..])
                    .expect("WithdrawHedgedJlp CBOR roundtrip");
            assert_eq!(
                decoded.jlp_lamports,
                u64::MAX,
                "WithdrawHedgedJlp MUST emit u64::MAX while hedgedjlp daemon \
                 unwind is all-or-nothing — see allocator_runner.rs comment \
                 and DEVLOG rc15"
            );
        }
    }

    #[test]
    fn missing_target_for_strategy_errors() {
        let mut t = targets();
        t.hedgedjlp = None;
        let a = AllocatorAction::Deposit {
            strategy: "hedgedjlp".into(),
            amount_usd: 50.0,
            reason: "test".into(),
        };
        let err = action_to_envelope_spec(&a, &t).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("hedgedjlp"));
    }
}
