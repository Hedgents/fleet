//! Async wrapper around a single SQLite connection.

use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
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

-- rc50: invite-code-guarded vault waitlist. Codes seeded from the
-- HEDGENTS_INITIAL_INVITE_CODES env var on dashboard boot (idempotent).
-- Disable a code mid-beta by setting enabled=0; track who signed up via
-- invite_redemptions. Email is the only PII we record.
CREATE TABLE IF NOT EXISTS invite_codes (
    code TEXT PRIMARY KEY,
    label TEXT,
    created_at INTEGER NOT NULL,
    max_redemptions INTEGER NOT NULL DEFAULT 1,
    enabled INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS invite_redemptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    code TEXT NOT NULL,
    email TEXT NOT NULL,
    redeemed_at INTEGER NOT NULL,
    UNIQUE(code, email)
);
CREATE INDEX IF NOT EXISTS idx_invite_redemptions_code ON invite_redemptions(code);
"#;

/// rc44: out-of-band ALTER TABLE migrations. Each statement is wrapped
/// in `execute_or_skip_existing` because SQLite has no `ADD COLUMN IF
/// NOT EXISTS`. Used for additive schema changes that need to land on
/// already-running databases (where pre-rc44 rows must remain readable).
const MIGRATIONS: &[&str] = &[
    // rc44: realtime perp-position PnL from Jupiter's perps-api,
    // snapshotted alongside the existing per-strategy values. Signed
    // INTEGER micro-USD (perp losses are common; negative is first-class).
    "ALTER TABLE chain_aum_snapshots ADD COLUMN hedgedjlp_perps_pnl_after_fees_usd_micro INTEGER",
    // rc45: stable_yield cToken balance — paired with stable_yield_usd
    // (the live underlying value, × 1e6 = lamports) it gives the
    // current Kamino USDC reserve exchange rate. Snapshot baseline +
    // current → pure interest = ctoken × (rate_now - rate_first).
    "ALTER TABLE chain_aum_snapshots ADD COLUMN stable_yield_ctoken_balance INTEGER",
    // rc46: multiply jitoSOL cToken balance — paired with the new
    // multiply_jitosol_underlying_lamports column it gives the live
    // Kamino jitoSOL exchange rate; pure-interest math mirrors rc45's
    // stable_yield path.
    "ALTER TABLE chain_aum_snapshots ADD COLUMN multiply_jitosol_ctoken_balance INTEGER",
    // rc46: multiply jitoSOL underlying balance in raw jitoSOL lamports
    // (9 decimals). Lamports × jitoSOL price = USD collateral value.
    "ALTER TABLE chain_aum_snapshots ADD COLUMN multiply_jitosol_underlying_lamports INTEGER",
    // rc46: multiply SOL borrow principal in lamports (9 decimals).
    // Already includes Kamino's accumulated borrow-rate growth (derived
    // from `borrowed_amount_sf >> 60`), so the *delta* between two
    // snapshots == SOL interest paid (assuming no new borrows in between).
    "ALTER TABLE chain_aum_snapshots ADD COLUMN multiply_sol_borrowed_lamports INTEGER",
];

fn apply_migrations(conn: &Connection) -> Result<()> {
    for stmt in MIGRATIONS {
        match conn.execute(stmt, []) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                if msg.contains("duplicate column name") =>
            {
                // Already applied on a previous boot — expected.
            }
            Err(e) => {
                return Err(e).with_context(|| format!("migration: {stmt}"));
            }
        }
    }
    Ok(())
}

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
            apply_migrations(&conn).context("running additive migrations")?;
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
        hedgedjlp_perps_pnl_after_fees_usd_micro: Option<i64>,
        stable_yield_ctoken_balance: Option<i64>,
        multiply_jitosol_ctoken_balance: Option<i64>,
        multiply_jitosol_underlying_lamports: Option<i64>,
        multiply_sol_borrowed_lamports: Option<i64>,
    ) -> Result<()> {
        let conn = self.inner.lock().await;
        conn.execute(
            "INSERT INTO chain_aum_snapshots
                (ts_unix, total_usd, multiply_usd, stable_yield_usd,
                 hedgedjlp_jlp_usd, hedgedjlp_collateral_usd, idle_usd,
                 hedgedjlp_perps_pnl_after_fees_usd_micro,
                 stable_yield_ctoken_balance,
                 multiply_jitosol_ctoken_balance,
                 multiply_jitosol_underlying_lamports,
                 multiply_sol_borrowed_lamports)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(ts_unix) DO NOTHING",
            params![
                ts_unix,
                total_usd,
                multiply_usd,
                stable_yield_usd,
                hedgedjlp_jlp_usd,
                hedgedjlp_collateral_usd,
                idle_usd,
                hedgedjlp_perps_pnl_after_fees_usd_micro,
                stable_yield_ctoken_balance,
                multiply_jitosol_ctoken_balance,
                multiply_jitosol_underlying_lamports,
                multiply_sol_borrowed_lamports,
            ],
        )?;
        Ok(())
    }

    /// All chain-AUM snapshots taken at-or-after `cutoff_unix`, oldest
    /// first. /pnl uses these to bracket a time window and compute
    /// deltas without trusting per-daemon telemetry.
    pub async fn chain_aum_snapshots_since(&self, cutoff_unix: i64) -> Result<Vec<ChainAumRow>> {
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
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM chain_aum_snapshots", [], |row| {
            row.get(0)
        })?;
        Ok(n as u64)
    }

    /// rc43: per-strategy "first non-zero observed value" baselines.
    /// Used to compute lifetime unrealised earnings (current minus
    /// first-observed). Each strategy is tracked independently — the
    /// timestamps may differ if positions were opened on different
    /// days. Returns `Some(value)` for any strategy where the snapshot
    /// table has ever recorded a positive value, `None` otherwise.
    ///
    /// Note: this is a "delta since position opened" metric, not pure
    /// interest accrual. Subsequent allocator-driven deposits and
    /// withdraws into a strategy are mixed into the delta and the
    /// frontend signals this with a "since position opened" caveat.
    pub async fn first_nonzero_per_strategy(&self) -> Result<FirstNonzeroPerStrategy> {
        let conn = self.inner.lock().await;
        let multiply = conn
            .query_row(
                "SELECT multiply_usd, ts_unix FROM chain_aum_snapshots
                 WHERE multiply_usd > 0 ORDER BY ts_unix ASC LIMIT 1",
                [],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let stable_yield = conn
            .query_row(
                "SELECT stable_yield_usd, ts_unix FROM chain_aum_snapshots
                 WHERE stable_yield_usd > 0 ORDER BY ts_unix ASC LIMIT 1",
                [],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        // hedgedjlp's deployed surface = JLP + collateral. Track each
        // leg's first non-zero independently; frontend sums them.
        let hedgedjlp_jlp = conn
            .query_row(
                "SELECT hedgedjlp_jlp_usd, ts_unix FROM chain_aum_snapshots
                 WHERE hedgedjlp_jlp_usd > 0 ORDER BY ts_unix ASC LIMIT 1",
                [],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let hedgedjlp_collateral = conn
            .query_row(
                "SELECT hedgedjlp_collateral_usd, ts_unix FROM chain_aum_snapshots
                 WHERE hedgedjlp_collateral_usd > 0 ORDER BY ts_unix ASC LIMIT 1",
                [],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let total = conn
            .query_row(
                "SELECT total_usd, ts_unix FROM chain_aum_snapshots
                 WHERE total_usd > 0 ORDER BY ts_unix ASC LIMIT 1",
                [],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(FirstNonzeroPerStrategy {
            multiply,
            stable_yield,
            hedgedjlp_jlp,
            hedgedjlp_collateral,
            total,
        })
    }
}

/// rc43: first non-zero observed value + timestamp per strategy.
/// `None` means the strategy has never held capital while this
/// dashboard server has been running.
#[derive(Debug, Clone, Default)]
pub struct FirstNonzeroPerStrategy {
    pub multiply: Option<(f64, i64)>,
    pub stable_yield: Option<(f64, i64)>,
    pub hedgedjlp_jlp: Option<(f64, i64)>,
    pub hedgedjlp_collateral: Option<(f64, i64)>,
    pub total: Option<(f64, i64)>,
}

impl Store {
    /// rc45: first snapshot row that recorded a non-zero
    /// `stable_yield_ctoken_balance`. Used to anchor pure interest
    /// accrual: `current_ctoken × (current_rate - baseline_rate)`
    /// where rate = `stable_yield_usd × 1e6 / stable_yield_ctoken_balance`
    /// (lamports per cToken).
    ///
    /// Returns `None` when no row has yet been snapshotted with the
    /// rc45-added `stable_yield_ctoken_balance` column populated —
    /// either pre-rc45 history only, or no stable_yield deposit
    /// observed.
    pub async fn first_stable_yield_baseline(&self) -> Result<Option<StableYieldBaseline>> {
        let conn = self.inner.lock().await;
        let row = conn
            .query_row(
                "SELECT ts_unix, stable_yield_usd, stable_yield_ctoken_balance
                 FROM chain_aum_snapshots
                 WHERE stable_yield_ctoken_balance IS NOT NULL
                   AND stable_yield_ctoken_balance > 0
                 ORDER BY ts_unix ASC LIMIT 1",
                [],
                |row| {
                    Ok(StableYieldBaseline {
                        ts_unix: row.get::<_, i64>(0)?,
                        underlying_usd: row.get::<_, f64>(1)?,
                        ctoken_balance: row.get::<_, i64>(2)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }
}

/// rc45: baseline anchor for stable_yield pure-interest accrual.
#[derive(Debug, Clone, Copy)]
pub struct StableYieldBaseline {
    pub ts_unix: i64,
    pub underlying_usd: f64,
    pub ctoken_balance: u64,
}

impl Store {
    /// rc46: first snapshot row that recorded a non-zero
    /// `multiply_jitosol_ctoken_balance` AND a non-zero
    /// `multiply_sol_borrowed_lamports`. Anchors two-sided multiply
    /// pure-interest accrual: collateral side via the same
    /// `ctoken × Δrate` math as rc45 stable_yield; borrow side via
    /// `current_borrowed - baseline_borrowed` (a positive delta is
    /// interest paid when no new borrows landed).
    ///
    /// Returns `None` when no row has yet been snapshotted with both
    /// fields populated — either pre-rc46 history only, or multiply has
    /// no open obligation.
    pub async fn first_multiply_baseline(&self) -> Result<Option<MultiplyBaseline>> {
        let conn = self.inner.lock().await;
        let row = conn
            .query_row(
                "SELECT ts_unix,
                        multiply_jitosol_ctoken_balance,
                        multiply_jitosol_underlying_lamports,
                        multiply_sol_borrowed_lamports
                 FROM chain_aum_snapshots
                 WHERE multiply_jitosol_ctoken_balance IS NOT NULL
                   AND multiply_jitosol_ctoken_balance > 0
                   AND multiply_jitosol_underlying_lamports IS NOT NULL
                   AND multiply_jitosol_underlying_lamports > 0
                   AND multiply_sol_borrowed_lamports IS NOT NULL
                 ORDER BY ts_unix ASC LIMIT 1",
                [],
                |row| {
                    Ok(MultiplyBaseline {
                        ts_unix: row.get::<_, i64>(0)?,
                        jitosol_ctoken_balance: row.get::<_, i64>(1)? as u64,
                        jitosol_underlying_lamports: row.get::<_, i64>(2)? as u64,
                        sol_borrowed_lamports: row.get::<_, i64>(3)? as u64,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }
}

/// rc46: baseline anchor for multiply pure-interest accrual.
#[derive(Debug, Clone, Copy)]
pub struct MultiplyBaseline {
    pub ts_unix: i64,
    pub jitosol_ctoken_balance: u64,
    pub jitosol_underlying_lamports: u64,
    pub sol_borrowed_lamports: u64,
}

/// rc50: validation result for an invite code lookup.
#[derive(Debug, Clone)]
pub enum InviteValidation {
    /// Code is valid and has remaining capacity.
    Valid { remaining: i64 },
    /// Code exists but is disabled or fully redeemed.
    Exhausted,
    /// Code doesn't exist.
    Unknown,
}

impl Store {
    /// rc50: idempotently insert an invite code. Called at dashboard
    /// boot for each entry in `HEDGENTS_INITIAL_INVITE_CODES` so codes
    /// can be rotated by editing the env file + restart. Existing
    /// rows (matched by code) are NOT updated — `max_redemptions` and
    /// `enabled` stay whatever the operator most recently set.
    pub async fn upsert_invite_code(
        &self,
        code: &str,
        label: Option<&str>,
        max_redemptions: i64,
    ) -> Result<()> {
        let conn = self.inner.lock().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO invite_codes (code, label, created_at, max_redemptions, enabled)
             VALUES (?1, ?2, ?3, ?4, 1)
             ON CONFLICT(code) DO NOTHING",
            params![code, label, now, max_redemptions],
        )?;
        Ok(())
    }

    /// rc50: look up a code and report whether it's redeemable. Pure
    /// read — does not consume capacity. `Valid { remaining }` returns
    /// the count of redemptions still available (max - count(uses)).
    pub async fn validate_invite_code(&self, code: &str) -> Result<InviteValidation> {
        let conn = self.inner.lock().await;
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT max_redemptions, enabled FROM invite_codes WHERE code = ?1",
                params![code],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((max_redemptions, enabled)) = row else {
            return Ok(InviteValidation::Unknown);
        };
        if enabled == 0 {
            return Ok(InviteValidation::Exhausted);
        }
        let used: i64 = conn.query_row(
            "SELECT COUNT(*) FROM invite_redemptions WHERE code = ?1",
            params![code],
            |row| row.get(0),
        )?;
        let remaining = max_redemptions - used;
        if remaining <= 0 {
            Ok(InviteValidation::Exhausted)
        } else {
            Ok(InviteValidation::Valid { remaining })
        }
    }

    /// rc50: atomically validate + redeem. Returns `Ok(true)` on first
    /// redemption, `Ok(false)` if the (code, email) pair was already
    /// redeemed (idempotent — duplicate submits are no-ops). Errors if
    /// the code is unknown/exhausted (caller should have validated
    /// first via `validate_invite_code` and shown a UI message; this
    /// is the second gate to prevent a race between two browser tabs).
    pub async fn redeem_invite_code(&self, code: &str, email: &str) -> Result<bool> {
        let conn = self.inner.lock().await;
        // Re-validate inside the same lock to close the race window.
        let row: Option<(i64, i64)> = conn
            .query_row(
                "SELECT max_redemptions, enabled FROM invite_codes WHERE code = ?1",
                params![code],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((max_redemptions, enabled)) = row else {
            bail!("invite code does not exist");
        };
        if enabled == 0 {
            bail!("invite code is disabled");
        }
        let used: i64 = conn.query_row(
            "SELECT COUNT(*) FROM invite_redemptions WHERE code = ?1",
            params![code],
            |row| row.get(0),
        )?;
        // Check idempotency: same (code, email) already redeemed?
        let already_redeemed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM invite_redemptions WHERE code = ?1 AND email = ?2",
            params![code, email],
            |row| row.get(0),
        )?;
        if already_redeemed > 0 {
            return Ok(false);
        }
        if used >= max_redemptions {
            bail!("invite code fully redeemed");
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        conn.execute(
            "INSERT INTO invite_redemptions (code, email, redeemed_at)
             VALUES (?1, ?2, ?3)",
            params![code, email, now],
        )?;
        Ok(true)
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
