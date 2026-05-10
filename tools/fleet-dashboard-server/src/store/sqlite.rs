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
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("creating parent dir {}", parent.display())
                    })?;
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
        let id = conn.query_row(
            "INSERT OR IGNORE INTO pnl_snapshots (ts_unix, daemon, raw_json)
             VALUES (?1, ?2, ?3) RETURNING id",
            params![ts_unix as i64, daemon, raw_json],
            |row| row.get::<_, i64>(0),
        ).unwrap_or(0);
        Ok(id)
    }

    /// Recent mesh events filtered by `ts_ms >= since_ms`, newest first.
    pub async fn recent_events(&self, since_ms: i64, limit: usize) -> Result<Vec<MeshEvent>> {
        self.recent_events_filtered(since_ms, limit, None, None, false).await
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

    /// Latest beacon timestamp (ts_ms) per role.
    /// Returns rows of (sender_role, max_ts_ms).
    pub async fn last_beacon_ts_by_role(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.inner.lock().await;
        let mut stmt = conn.prepare(
            "SELECT sender_role, MAX(ts_ms) FROM mesh_events
             WHERE msg_type = 'Beacon'
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

    /// Total count of mesh events. Useful for sanity tests.
    pub async fn event_count(&self) -> Result<u64> {
        let conn = self.inner.lock().await;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM mesh_events", [], |row| row.get(0))?;
        Ok(n as u64)
    }
}
