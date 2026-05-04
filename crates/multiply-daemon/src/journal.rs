use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;
use std::sync::Mutex;
use tracing::info;

pub struct Journal {
    conn: Mutex<Connection>,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ixns (
                id        INTEGER PRIMARY KEY AUTOINCREMENT,
                position  TEXT NOT NULL,
                payload   BLOB NOT NULL,
                state     TEXT NOT NULL CHECK(state IN ('pending','submitted','confirmed','failed')),
                signature TEXT,
                created   INTEGER NOT NULL,
                updated   INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS ixns_state_idx ON ixns(state);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// On boot, log every non-confirmed ixn so a human (or the strategy
    /// follow-up plan) can decide what to do.
    pub async fn replay(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, position, state FROM ixns WHERE state != 'confirmed'")?;
        let rows: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .filter_map(|r| r.ok())
            .collect();
        for (id, pos, state) in rows {
            info!(id, position = %pos, state = %state, "journal replay: orphan ixn");
        }
        Ok(())
    }
}
