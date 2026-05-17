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

/// Owned wrapper around the audit log path. Created at boot; the daemon
/// refuses to start if the path is not writable (audit-log unavailability
/// is treated as fatal — the operator should know immediately, not after
/// an unlogged decision).
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
    pub fn append(
        &self,
        mode: &str,
        snap: &FleetSnapshot,
        action: &AllocatorAction,
    ) -> Result<()> {
        self.append_with_result(mode, snap, action, "")
    }

    /// Write one JSONL line, populating `envelope_result` with the
    /// per-tick dispatch outcome. Execute mode uses this to record
    /// `"sent"`, `"failed:<reason>"`, or `"skipped:<reason>"`.
    pub fn append_with_result(
        &self,
        mode: &str,
        snap: &FleetSnapshot,
        action: &AllocatorAction,
        envelope_result: &str,
    ) -> Result<()> {
        let rec = AuditRecord {
            ts_unix: now_unix(),
            mode,
            snapshot: AuditSnapshot::from(snap),
            action,
            envelope_result: envelope_result.to_string(),
        };
        append_audit(&self.path, &rec)
    }
}
