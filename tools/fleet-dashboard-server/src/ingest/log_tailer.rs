//! Watches a directory for `*.log` files and streams new JSON lines as
//! they're appended. One tokio task owns the watcher + a per-file offset
//! map; on each notify event it tails the affected files, parses each
//! complete newline-terminated line as JSON, and emits a `RawLogLine`
//! through the supplied mpsc channel.
//!
//! Resilience:
//! - On startup, every existing `*.log` file is walked from offset 0.
//! - If a file is truncated (rotated), the offset resets to 0.
//! - Non-JSON lines (tracing's text formatter, panic stack traces) are
//!   silently dropped — we only ingest structured JSON events.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{recommended_watcher, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::types::RawLogLine;

/// Drive a directory watcher forever, emitting `RawLogLine`s to `tx`.
///
/// Returns only on unrecoverable error (e.g. cannot install the watcher).
pub async fn run(dir: PathBuf, tx: mpsc::Sender<RawLogLine>) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating log dir {}", dir.display()))?;
    }

    // Bridge the sync notify::Watcher callback into an async-friendly
    // tokio mpsc.
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<notify::Event>();

    let mut watcher = recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(ev) => {
                let _ = notify_tx.send(ev);
            }
            Err(e) => warn!(?e, "notify watcher error"),
        }
    })
    .context("creating notify watcher")?;

    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", dir.display()))?;

    let mut offsets: HashMap<PathBuf, u64> = HashMap::new();

    // Initial sweep: pick up anything that's already on disk.
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            let p = entry.path();
            if is_log_path(&p) {
                if let Err(e) = tail_one(&p, &mut offsets, &tx).await {
                    debug!(?e, ?p, "initial tail failed (file may have vanished)");
                }
            }
        }
    }

    // Main event loop. We coalesce bursts of fs events with a short
    // settle window, then re-scan all known logs.
    let settle = Duration::from_millis(50);
    loop {
        let Some(_first) = notify_rx.recv().await else {
            warn!("notify channel closed, log_tailer exiting");
            return Ok(());
        };

        // Drain anything that piled up during the brief settle.
        tokio::time::sleep(settle).await;
        while notify_rx.try_recv().is_ok() {}

        if let Ok(read) = std::fs::read_dir(&dir) {
            for entry in read.flatten() {
                let p = entry.path();
                if is_log_path(&p) {
                    if let Err(e) = tail_one(&p, &mut offsets, &tx).await {
                        debug!(?e, ?p, "tail failed");
                    }
                }
            }
        }

        // Drop files that no longer exist so we don't leak offsets across
        // log rotations.
        offsets.retain(|p, _| p.exists());

        let _ = _first;

        // Bridge: if downstream went away there's nothing for us to do.
        if tx.is_closed() {
            return Ok(());
        }
    }
}

#[allow(dead_code)]
fn _silence_event_kind(_e: EventKind) {}

fn is_log_path(p: &Path) -> bool {
    p.is_file()
        && p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("log"))
            .unwrap_or(false)
}

/// Read everything new in `path`, parse each line as JSON, and emit.
async fn tail_one(
    path: &Path,
    offsets: &mut HashMap<PathBuf, u64>,
    tx: &mpsc::Sender<RawLogLine>,
) -> Result<()> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?;
    let size = meta.len();
    let prev = offsets.get(path).copied().unwrap_or(0);

    // Rotated / truncated.
    let start = if size < prev { 0 } else { prev };
    if start == size {
        return Ok(());
    }

    let bytes =
        std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if (start as usize) >= bytes.len() {
        offsets.insert(path.to_path_buf(), bytes.len() as u64);
        return Ok(());
    }
    let slice = &bytes[start as usize..];

    // Find the last complete newline; carry over any trailing partial
    // line by leaving the offset before it.
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
            Ok(s) => s,
            Err(_) => continue,
        };
        let trimmed = s.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        let raw: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let item = RawLogLine {
            source_file: path.to_path_buf(),
            raw,
        };
        if tx.send(item).await.is_err() {
            return Ok(());
        }
    }

    Ok(())
}
