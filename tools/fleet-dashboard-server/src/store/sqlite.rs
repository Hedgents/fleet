//! Async wrapper around a single SQLite connection.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::types::{Direction, MeshEvent};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS mesh_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_unix INTEGER NOT NULL,
    ts_ms INTEGER NOT NULL,
    sender_role TEXT NOT NULL,
    direction TEXT NOT NULL,
    msg_type TEXT NOT NULL,
    payload_summary TEXT NOT NULL,
    payload_json TEXT,
    conv_id TEXT,
    tx_signature TEXT
);
CREATE INDEX IF NOT EXISTS idx_mesh_events_ts ON mesh_events(ts_ms DESC);
CREATE INDEX IF NOT EXISTS idx_mesh_events_role ON mesh_events(sender_role);
CREATE INDEX IF NOT EXISTS idx_mesh_events_msg ON mesh_events(msg_type);

CREATE TABLE IF NOT EXISTS pnl_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_unix INTEGER NOT NULL,
    daemon TEXT NOT NULL,
    raw_json TEXT NOT NULL,
    UNIQUE(daemon, ts_unix)
);
CREATE INDEX IF NOT EXISTS idx_pnl_ts ON pnl_snapshots(ts_unix DESC);
CREATE INDEX IF NOT EXISTS idx_pnl_daemon ON pnl_snapshots(daemon);

CREATE TABLE IF NOT EXISTS apr_samples (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts_ms INTEGER NOT NULL,
    strategy TEXT NOT NULL,
    apr_bps INTEGER NOT NULL,
    source TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_apr_strategy_ts ON apr_samples(strategy, ts_ms DESC);
CREATE INDEX IF NOT EXISTS idx_apr_ts ON apr_samples(ts_ms DESC);

-- rc24: chain-state AUM snapshots, the canonical source of truth for /pnl.
--
-- Pre-rc24 /pnl was computed by aggregating per-daemon pnl_snapshots
-- telemetry rows. That worked when every daemon was running in
-- execute mode and writing valid rows — but it failed silently when:
--   (1) a daemon's internal `ActivePosition` desynced from chain
--       (e.g. after a partial unwind), causing its rows to report
--       zero even though the chain still held the position;
--   (2) a strategy was run in paper-mode units whose telemetry lives
--       in `*-pnl.jsonl`, not the `*-live-pnl.jsonl` paths the /pnl
--       handler scanned;
--   (3) a strategy hadn't booted yet for the window in question.
--
-- The dashboard already has authoritative chain reads (kamino,
-- jupiter_perps, balance) which power /aum. rc24 snapshots that same
-- read into this table on a 60s cadence so /pnl can compute deltas
-- against ground truth.
CREATE TABLE IF NOT EXISTS chain_aum_snapshots (
    ts_unix INTEGER PRIMARY KEY,
    total_usd REAL NOT NULL,
    multiply_usd REAL NOT NULL,
    stable_yield_usd REAL NOT NULL,
    hedgedjlp_jlp_usd REAL NOT NULL,
    hedgedjlp_collateral_usd REAL NOT NULL,
    idle_usd REAL NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_chain_aum_ts ON chain_aum_snapshots(ts_unix DESC);
"#;

#[derive(Clone)]
pub struct Store {
    inner: Arc<Mutex<Connection>>,
}

impl Store {
    /// Open (or create) the SQLite database at `path` and run schema
    /// migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating parent dir {}", parent.display()))?;
                }
            }
            let conn = Connection::open(&path)
                .with_context(|| format!("opening sqlite at {}", path.display()))?;
            conn.execute_batch(SCHEMA)
                .context("running mesh_events / pnl_snapshots schema")?;
            Ok(conn)
        })
        .await
        .context("spawn_blocking join")??;

        Ok(Self {
            inner: Arc::new(Mutex::new(conn)),
        })
    }

    /// Insert a single mesh event, returning its row id.
    pub async fn insert_mesh_event(&self, event: &MeshEvent) -> Result<i64> {
        let conn = self.inner.lock().await;
        let id = conn.query_row(
            "INSERT INTO mesh_events (
                ts_unix, ts_ms, sender_role, direction, msg_type,
                payload_summary, payload_json, conv_id, tx_signature
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             RETURNING id",
            params![
                event.ts_unix,
                event.ts_ms,
                event.sender_role,
                event.direction.as_str(),
                event.msg_type,
                event.payload_summary,
                event.payload_json,
                event.conv_id,
                event.tx_signature,
            ],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(id)
    }

    /// Insert a single PnL snapshot row from a JSONL line.
    pub async fn insert_pnl_snapshot(
        &self,
        daemon: &str,
        ts_unix: u64,
        raw_json: &str,
    ) -> Result<i64> {
        let conn = self.inner.lock().await;
        let id = conn
            .query_row(
                "INSERT OR IGNORE INTO pnl_snapshots (ts_unix, daemon, raw_json)
             VALUES (?1, ?2, ?3) RETURNING id",
                params![ts_unix as i64, daemon, raw_json],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0);
        Ok(id)
    }

    /// Recent mesh events filtered by `ts_ms >= since_ms`, newest first.
    pub async fn recent_events(&self, since_ms: i64, limit: usize) -> Result<Vec<MeshEvent>> {
        self.recent_events_filtered(since_ms, limit, None, None, false)
            .await
    }

    /// Recent mesh events with optional `role` and `msg_type` filters.
    /// Newest first, capped at `limit` rows.
    pub async fn recent_events_filtered(
        &self,
        since_ms: i64,
        limit: usize,
        role: Option<&str>,
        msg_type: Option<&str>,
        exclude_beacons: bool,
    ) -> Result<Vec<MeshEvent>> {
        let conn = self.inner.lock().await;
        // Build SQL dynamically based on which optional filters are set.
        // We bind in this fixed order: since_ms, [role], [msg_type], limit.
        let mut sql = String::from(
            "SELECT id, ts_unix, ts_ms, sender_role, direction, msg_type,
                    payload_summary, payload_json, conv_id, tx_signature
             FROM mesh_events
             WHERE ts_ms >= ?1",
        );
        let mut next_idx = 2usize;
        if role.is_some() {
            sql.push_str(&format!(" AND sender_role = ?{}", next_idx));
            next_idx += 1;
        }
        if msg_type.is_some() {
            sql.push_str(&format!(" AND msg_type = ?{}", next_idx));
            next_idx += 1;
        }
        if exclude_beacons {
            sql.push_str(" AND msg_type != 'Beacon'");
        }
        sql.push_str(&format!(" ORDER BY ts_ms DESC LIMIT ?{}", next_idx));

        let mut stmt = conn.prepare(&sql)?;
        // Collect bound params as &dyn ToSql.
        let limit_i: i64 = limit as i64;
        let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(4);
        bound.push(&since_ms);
        if let Some(r) = role.as_ref() {
            bound.push(r);
        }
        if let Some(m) = msg_type.as_ref() {
            bound.push(m);
        }
        bound.push(&limit_i);
        let rows = stmt.query_map(rusqlite::params_from_iter(bound), |row| {
            let dir_s: String = row.get(4)?;
            let direction = Direction::parse(&dir_s).unwrap_or(Direction::Internal);
            Ok(MeshEvent {
                id: Some(row.get(0)?),
                ts_unix: row.get(1)?,
                ts_ms: row.get(2)?,
                sender_role: row.get(3)?,
                direction,
                msg_type: row.get(5)?,
                payload_summary: row.get(6)?,
                payload_json: row.get(7)?,
                conv_id: row.get(8)?,
                tx_signature: row.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Latest activity timestamp (ts_ms) per role — any message type, not just
    /// Beacons. This prevents false-red when a daemon's Beacon task dies but
    /// it continues processing inbound messages (Assign → Report cycles).
    pub async fn last_beacon_ts_by_role(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.inner.lock().await;
        let mut stmt = conn.prepare(
            "SELECT sender_role, MAX(ts_ms) FROM mesh_events
             GROUP BY sender_role",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Hourly activity buckets for the dashboard timeline.
    ///
    /// Returns one row per hour in the window `[now - hours, now]`, oldest
    /// first. Beacons are excluded — the timeline shows actionable mesh
    /// activity (Assigns, Reports, MarketSignals, Escalates), not health
    /// pulses. Empty hours are included with `events = 0` so the chart's
    /// x-axis is dense.
    pub async fn activity_buckets_ms(&self, hours: u32) -> Result<Vec<(i64, u64)>> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let hour_ms: i64 = 3_600_000;
        let bucket_start_ms = (now_ms / hour_ms) * hour_ms;
        let window_start_ms = bucket_start_ms - (hours as i64 - 1) * hour_ms;

        let conn = self.inner.lock().await;
        let mut stmt = conn.prepare(
            "SELECT (ts_ms / 3600000) * 3600000 AS bucket_ms, COUNT(*) AS n
             FROM mesh_events
             WHERE ts_ms >= ? AND msg_type != 'Beacon'
             GROUP BY bucket_ms",
        )?;
        let mut counts: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();
        let rows = stmt.query_map([window_start_ms], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        for r in rows {
            let (ts, n) = r?;
            counts.insert(ts, n);
        }
        let mut out = Vec::with_capacity(hours as usize);
        for i in 0..(hours as i64) {
            let ts = window_start_ms + i * hour_ms;
            out.push((ts, counts.get(&ts).copied().unwrap_or(0)));
        }
        Ok(out)
    }

    /// Most recent N pnl_snapshot rows for a given daemon, oldest first.
    pub async fn recent_pnl_for(&self, daemon: &str, limit: usize) -> Result<Vec<(i64, String)>> {
        let conn = self.inner.lock().await;
        let mut stmt = conn.prepare(
            "SELECT ts_unix, raw_json FROM pnl_snapshots
             WHERE daemon = ?1
             ORDER BY ts_unix DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![daemon, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out.reverse();
        Ok(out)
    }

    /// Most recent on-chain signature emitted by `role`, if any. The
    /// `mesh_events.tx_signature` column is populated from the daemon's
    /// JSON tracing `tx` / `tx_signature` fields by the envelope decoder;
    /// rows where it is non-null correspond to confirmed on-chain
    /// transactions. Used by `/strategies` to render the "View on-chain →"
    /// link on each card.
    pub async fn last_sig_for_role(&self, role: &str) -> Result<Option<String>> {
        let conn = self.inner.lock().await;
        let row: rusqlite::Result<String> = conn.query_row(
            "SELECT tx_signature FROM mesh_events
             WHERE sender_role = ?1 AND tx_signature IS NOT NULL
             ORDER BY ts_ms DESC LIMIT 1",
            params![role],
            |row| row.get(0),
        );
        match row {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Most recent N mesh events that carry a non-null `tx_signature`,
    /// newest first. Powers `/onchain/activity`.
    pub async fn recent_onchain_events(&self, limit: usize) -> Result<Vec<MeshEvent>> {
        let conn = self.inner.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, ts_unix, ts_ms, sender_role, direction, msg_type,
                    payload_summary, payload_json, conv_id, tx_signature
             FROM mesh_events
             WHERE tx_signature IS NOT NULL
             ORDER BY ts_ms DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let dir_s: String = row.get(4)?;
            let direction = Direction::parse(&dir_s).unwrap_or(Direction::Internal);
            Ok(MeshEvent {
                id: Some(row.get(0)?),
                ts_unix: row.get(1)?,
                ts_ms: row.get(2)?,
                sender_role: row.get(3)?,
                direction,
                msg_type: row.get(5)?,
                payload_summary: row.get(6)?,
                payload_json: row.get(7)?,
                conv_id: row.get(8)?,
                tx_signature: row.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Insert one APR sample. `ts_ms` is wall-clock ms; `apr_bps` is
    /// signed basis points (negative APR is meaningful for some delta-
    /// neutral strategies under adverse borrow-rate conditions).
    pub async fn insert_apr_sample(
        &self,
        ts_ms: i64,
        strategy: &str,
        apr_bps: i64,
        source: &str,
    ) -> Result<i64> {
        let conn = self.inner.lock().await;
        let id = conn.query_row(
            "INSERT INTO apr_samples (ts_ms, strategy, apr_bps, source)
             VALUES (?1, ?2, ?3, ?4) RETURNING id",
            params![ts_ms, strategy, apr_bps, source],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(id)
    }

    /// All APR samples for `strategy` within the last `hours` hours,
    /// oldest first. Returns `(ts_ms, apr_bps)` pairs.
    pub async fn apr_samples_for(&self, strategy: &str, hours: u32) -> Result<Vec<(i64, i64)>> {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let since_ms = now_ms - (hours as i64) * 3_600_000;
        let conn = self.inner.lock().await;
        let mut stmt = conn.prepare(
            "SELECT ts_ms, apr_bps FROM apr_samples
             WHERE strategy = ?1 AND ts_ms >= ?2
             ORDER BY ts_ms ASC",
        )?;
        let rows = stmt.query_map(params![strategy, since_ms], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Total count of mesh events. Useful for sanity tests.
    pub async fn event_count(&self) -> Result<u64> {
        let conn = self.inner.lock().await;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM mesh_events", [], |row| row.get(0))?;
        Ok(n as u64)
    }

    /// Insert one chain-AUM snapshot (rc24). `ON CONFLICT (ts_unix) DO
    /// NOTHING` makes the call idempotent — operators who hand-call
    /// the sampler at boot won't double-insert if it races the
    /// scheduled tick.
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_chain_aum_snapshot(
        &self,
        ts_unix: i64,
        total_usd: f64,
        multiply_usd: f64,
        stable_yield_usd: f64,
        hedgedjlp_jlp_usd: f64,
        hedgedjlp_collateral_usd: f64,
        idle_usd: f64,
    ) -> Result<()> {
        let conn = self.inner.lock().await;
        conn.execute(
            "INSERT INTO chain_aum_snapshots
                (ts_unix, total_usd, multiply_usd, stable_yield_usd,
                 hedgedjlp_jlp_usd, hedgedjlp_collateral_usd, idle_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(ts_unix) DO NOTHING",
            params![
                ts_unix,
                total_usd,
                multiply_usd,
                stable_yield_usd,
                hedgedjlp_jlp_usd,
                hedgedjlp_collateral_usd,
                idle_usd,
            ],
        )?;
        Ok(())
    }

    /// All chain-AUM snapshots taken at-or-after `cutoff_unix`, oldest
    /// first. /pnl uses these to bracket a time window and compute
    /// deltas without trusting per-daemon telemetry.
    pub async fn chain_aum_snapshots_since(
        &self,
        cutoff_unix: i64,
    ) -> Result<Vec<ChainAumRow>> {
        let conn = self.inner.lock().await;
        let mut stmt = conn.prepare(
            "SELECT ts_unix, total_usd, multiply_usd, stable_yield_usd,
                    hedgedjlp_jlp_usd, hedgedjlp_collateral_usd, idle_usd
             FROM chain_aum_snapshots
             WHERE ts_unix >= ?1
             ORDER BY ts_unix ASC",
        )?;
        let rows = stmt.query_map(params![cutoff_unix], |row| {
            Ok(ChainAumRow {
                ts_unix: row.get(0)?,
                total_usd: row.get(1)?,
                multiply_usd: row.get(2)?,
                stable_yield_usd: row.get(3)?,
                hedgedjlp_jlp_usd: row.get(4)?,
                hedgedjlp_collateral_usd: row.get(5)?,
                idle_usd: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Count of stored chain-AUM snapshots. Useful for "no history
    /// yet" branches in /pnl and for tests.
    pub async fn chain_aum_snapshot_count(&self) -> Result<u64> {
        let conn = self.inner.lock().await;
        let n: i64 =
            conn.query_row("SELECT COUNT(*) FROM chain_aum_snapshots", [], |row| {
                row.get(0)
            })?;
        Ok(n as u64)
    }
}

/// One row of the `chain_aum_snapshots` table. Mirrors the on-the-wire
/// `/aum` shape so the /pnl handler can compute per-strategy deltas
/// without re-querying chain state.
#[derive(Debug, Clone)]
pub struct ChainAumRow {
    pub ts_unix: i64,
    pub total_usd: f64,
    pub multiply_usd: f64,
    pub stable_yield_usd: f64,
    pub hedgedjlp_jlp_usd: f64,
    pub hedgedjlp_collateral_usd: f64,
    pub idle_usd: f64,
}
