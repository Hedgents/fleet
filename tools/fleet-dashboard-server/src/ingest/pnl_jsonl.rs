//! Tails per-daemon JSONL telemetry files and inserts each line into the
//! `pnl_snapshots` table.
//!
//! Files matched (in `--telemetry-dir`):
//! - `multiply-pnl.jsonl` -> daemon `multiply`
//! - `stable-yield-pnl.jsonl` -> daemon `stable_yield`
//! - `hedgedjlp-pnl.jsonl` -> daemon `hedgedjlp`
//! - `riskwatcher-pnl.jsonl` -> daemon `riskwatcher`
//! - `researcher-signals.jsonl` -> daemon `researcher`
//!
//! The full JSON line is stored verbatim in `pnl_snapshots.raw_json`; a
//! `ts` field on the line, if present, becomes `ts_unix`. Schemas vary
//! across daemons and we don't try to normalize at ingest time — the
//! REST API (Day 2) will project per-daemon views.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{recommended_watcher, RecursiveMode, Watcher};
use serde_json::Value;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::store::Store;

/// Drive the JSONL watcher forever.
pub async fn run(dir: PathBuf, store: Arc<Store>) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating telemetry dir {}", dir.display()))?;
    }

    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<notify::Event>();
    let mut watcher = recommended_watcher(move |res: notify::Result<notify::Event>| match res {
        Ok(ev) => {
            let _ = notify_tx.send(ev);
        }
        Err(e) => warn!(?e, "pnl notify watcher error"),
    })
    .context("creating pnl notify watcher")?;

    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", dir.display()))?;

    let mut offsets: HashMap<PathBuf, u64> = HashMap::new();

    // Initial sweep: seed offsets at end-of-file. The `pnl_snapshots`
    // table has a `UNIQUE(daemon, ts_unix)` constraint that dedupes
    // re-ingestion, but with multi-MB telemetry files re-reading on every
    // restart still wastes I/O. Symmetric with `log_tailer::run`.
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            let p = entry.path();
            if daemon_for_path(&p).is_some() {
                if let Ok(meta) = std::fs::metadata(&p) {
                    offsets.insert(p, meta.len());
                }
            }
        }
    }

    let settle = Duration::from_millis(50);
    loop {
        let Some(_first) = notify_rx.recv().await else {
            warn!("pnl notify channel closed, exiting");
            return Ok(());
        };
        tokio::time::sleep(settle).await;
        while notify_rx.try_recv().is_ok() {}

        if let Ok(read) = std::fs::read_dir(&dir) {
            for entry in read.flatten() {
                let p = entry.path();
                if let Some(daemon) = daemon_for_path(&p) {
                    if let Err(e) = ingest_one(&p, daemon, &mut offsets, &store).await {
                        debug!(?e, ?p, "pnl ingest failed");
                    }
                }
            }
        }
        offsets.retain(|p, _| p.exists());
    }
}

/// Map a JSONL filename to a daemon name.
pub fn daemon_for_path(p: &Path) -> Option<&'static str> {
    let name = p.file_name()?.to_str()?;
    // Live daemons on the deployed VM write their telemetry to
    // `<role>-live-pnl.jsonl` to keep them on disk separate from the
    // paper-trade variants run under the systemd units. Both forms feed
    // the same role bucket in `pnl_snapshots`.
    match name {
        "multiply-pnl.jsonl" | "multiply-live-pnl.jsonl" => Some("multiply"),
        "stable-yield-pnl.jsonl" | "stable-yield-live-pnl.jsonl" => Some("stable_yield"),
        "hedgedjlp-pnl.jsonl" | "hedgedjlp-live-pnl.jsonl" => Some("hedgedjlp"),
        "riskwatcher-pnl.jsonl" | "riskwatcher-live-pnl.jsonl" => Some("riskwatcher"),
        "researcher-signals.jsonl" | "researcher-live-signals.jsonl" => Some("researcher"),
        _ => None,
    }
}

async fn ingest_one(
    path: &Path,
    daemon: &str,
    offsets: &mut HashMap<PathBuf, u64>,
    store: &Arc<Store>,
) -> Result<()> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let size = meta.len();
    let prev = offsets.get(path).copied().unwrap_or(0);

    let start = if size < prev { 0 } else { prev };
    if start == size {
        return Ok(());
    }

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if (start as usize) >= bytes.len() {
        offsets.insert(path.to_path_buf(), bytes.len() as u64);
        return Ok(());
    }
    let slice = &bytes[start as usize..];

    let last_nl = slice.iter().rposition(|b| *b == b'\n');
    let consumable_len = match last_nl {
        Some(idx) => idx + 1,
        None => 0,
    };
    if consumable_len == 0 {
        return Ok(());
    }
    let consumable = &slice[..consumable_len];
    let new_offset = start + consumable_len as u64;
    offsets.insert(path.to_path_buf(), new_offset);

    for line in consumable.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let s = match std::str::from_utf8(line) {
            Ok(s) => s.trim(),
            Err(_) => continue,
        };
        if s.is_empty() || !s.starts_with('{') {
            continue;
        }
        let v: Value = match serde_json::from_str(s) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ts_unix = v
            .get("ts")
            .and_then(Value::as_u64)
            .or_else(|| v.get("ts_unix").and_then(Value::as_u64))
            .or_else(|| v.get("timestamp_unix").and_then(Value::as_u64))
            .unwrap_or(0);
        if let Err(e) = store.insert_pnl_snapshot(daemon, ts_unix, s).await {
            warn!(?e, daemon, "insert_pnl_snapshot failed");
        }
    }

    Ok(())
}
