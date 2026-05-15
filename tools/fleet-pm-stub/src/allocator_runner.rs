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
}

impl AuditSnapshot {
    pub fn from(snap: &FleetSnapshot) -> Self {
        Self {
            total_aum_usd: snap.total_aum_usd,
            idle_usd: snap.idle_usd,
            strategies: snap
                .strategies
                .iter()
                .map(|s| AuditStrategy {
                    id: s.id.clone(),
                    deployed_usd: s.deployed_usd,
                    nominal_apr_bps: s.nominal_apr_bps,
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
}
