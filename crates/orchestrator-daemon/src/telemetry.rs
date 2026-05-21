//! Append-only JSONL audit log for orchestrator decisions.
//!
//! One record per tick: the snapshot the allocator saw, the action it
//! recommended, the mode (`dry-run` vs `execute`), and a wall-clock unix
//! timestamp. Reuses the audit-record shape from `fleet_pm_stub::allocator_runner`
//! so a dashboard or external auditor can replay decisions across both the
//! one-shot CLI invocations and the long-running daemon.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use fleet_pm_stub::allocator::AllocatorAction;
use fleet_pm_stub::allocator_runner::{
    append_audit, now_unix, AuditRecord, AuditSnapshot, FleetSnapshot,
};
use fleet_pm_stub::allocator_targets::TargetWeights;

/// Owned wrapper around the audit log path. Created at boot; the daemon
/// refuses to start if the path is not writable (audit-log unavailability
/// is treated as fatal — the operator should know immediately, not after
/// an unlogged decision).
///
/// rc22 (allocator v2 M5): the resolved-per-tick `TargetWeights` is
/// passed by the caller on each `append_with_result` call rather than
/// being stored on the AuditLog. This lets `AprWeighted` mode change
/// its target weights every tick (per the per-snapshot APR gaps)
/// without the audit log needing to re-resolve them. In greedy mode
/// callers pass `None`, exactly as before.
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// Construct + smoke-test the path. Opens the file in append-create
    /// mode and writes nothing, but verifies the parent directory is
    /// writable. Returns an error wrapping the underlying I/O cause so
    /// the operator gets a specific message ("permission denied",
    /// "no such file or directory", etc.) at boot.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        // Probe write access without touching contents — open in append
        // mode, then drop the handle. If the open fails we surface that
        // error rather than discovering it on the first tick.
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open orchestrator audit log at {}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write one JSONL line with an empty `envelope_result`. Used by
    /// the dry-run path where no envelope is ever emitted.
    /// `target_weights` is the resolved-per-tick weights vector
    /// (caller computes from TargetMode); `None` in greedy mode.
    pub fn append(
        &self,
        mode: &str,
        snap: &FleetSnapshot,
        action: &AllocatorAction,
        target_weights: Option<&TargetWeights>,
    ) -> Result<()> {
        self.append_with_result(mode, snap, action, "", target_weights)
    }

    /// Write one JSONL line, populating `envelope_result` with the
    /// per-tick dispatch outcome. Execute mode uses this to record
    /// `"sent"`, `"failed:<reason>"`, or `"skipped:<reason>"`.
    /// `target_weights` is the resolved-per-tick weights vector
    /// (caller computes from TargetMode); `None` in greedy mode.
    pub fn append_with_result(
        &self,
        mode: &str,
        snap: &FleetSnapshot,
        action: &AllocatorAction,
        envelope_result: &str,
        target_weights: Option<&TargetWeights>,
    ) -> Result<()> {
        let rec = AuditRecord {
            ts_unix: now_unix(),
            mode,
            snapshot: AuditSnapshot::from_with_targets(snap, target_weights),
            action,
            envelope_result: envelope_result.to_string(),
        };
        append_audit(&self.path, &rec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fleet_pm_stub::allocator::StrategyRate;

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

    fn unique_tmp_path(suffix: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "orch-audit-test-{}-{}.jsonl",
            std::process::id(),
            suffix
        ));
        // Defensive: drop any stale file from a previous failed run.
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn audit_log_greedy_mode_omits_target_and_drift() {
        // target_weights=None on append → every row must omit
        // target_weight and drift_bps (the M3 invariant applied via
        // the M5-restructured append API).
        let path = unique_tmp_path("greedy");
        let log = AuditLog::open(path.clone()).expect("open");
        let snap = three_strat_snap();
        let action = AllocatorAction::NoAction {
            reason: "test".to_string(),
        };
        log.append("dry-run", &snap, &action, None).expect("append");
        let body = std::fs::read_to_string(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        let parsed: serde_json::Value = serde_json::from_str(body.trim()).expect("json");
        for row in parsed["snapshot"]["strategies"].as_array().unwrap() {
            assert!(
                row.get("current_weight").is_some(),
                "greedy mode still emits current_weight: {row}"
            );
            assert!(
                row.get("target_weight").is_none(),
                "greedy mode must omit target_weight: {row}"
            );
            assert!(
                row.get("drift_bps").is_none(),
                "greedy mode must omit drift_bps: {row}"
            );
        }
    }

    #[test]
    fn audit_log_drift_mode_emits_full_weight_triple() {
        // target_weights=Some on append → every row must carry
        // current_weight, target_weight, AND drift_bps. This is the
        // M4/M5 wiring's load-bearing contract: the orchestrator
        // promises that any audit row in drift mode is a complete
        // forensic record of the picker's input.
        let targets = TargetWeights::new(0.30, 0.30, 0.40).expect("valid");
        let path = unique_tmp_path("drift");
        let log = AuditLog::open(path.clone()).expect("open");
        let snap = three_strat_snap();
        let action = AllocatorAction::NoAction {
            reason: "test".to_string(),
        };
        log.append("execute", &snap, &action, Some(&targets))
            .expect("append");
        let body = std::fs::read_to_string(&path).expect("read");
        let _ = std::fs::remove_file(&path);
        let parsed: serde_json::Value = serde_json::from_str(body.trim()).expect("json");
        let arr = parsed["snapshot"]["strategies"].as_array().unwrap();
        for row in arr {
            assert!(
                row["current_weight"].is_number(),
                "drift mode requires current_weight: {row}"
            );
            assert!(
                row["target_weight"].is_number(),
                "drift mode requires target_weight: {row}"
            );
            assert!(
                row["drift_bps"].is_number(),
                "drift mode requires drift_bps: {row}"
            );
        }
        // Spot-check: stable_yield is at-target (current=0.30=target) →
        // drift_bps=0.
        let stable = arr
            .iter()
            .find(|r| r["id"] == "stable_yield")
            .expect("stable");
        assert_eq!(stable["drift_bps"].as_i64().unwrap(), 0);
    }
}
