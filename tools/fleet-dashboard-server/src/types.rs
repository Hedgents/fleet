//! Shared types for the fleet-dashboard-server.
//!
//! `MeshEvent` is the canonical row for the `mesh_events` SQLite table.
//! `RawLogLine` is the wire shape between the log_tailer and the
//! envelope_decoder.

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct MeshEvent {
    /// Row id, `None` before insert, `Some(id)` after.
    pub id: Option<i64>,
    /// Unix seconds.
    pub ts_unix: i64,
    /// Unix milliseconds (from log timestamp).
    pub ts_ms: i64,
    /// One of `multiply`, `stable_yield`, `hedgedjlp`, `riskwatcher`,
    /// `researcher`, `orchestrator`.
    pub sender_role: String,
    pub direction: Direction,
    /// `Beacon`, `Assign`, `Approve`, `Withdraw`, `Report`, `Escalate`,
    /// `MarketSignal`, `Internal`.
    pub msg_type: String,
    /// Pre-decoded human-readable sentence for the dashboard feed.
    pub payload_summary: String,
    /// Original raw fields JSON for click-to-expand.
    pub payload_json: Option<String>,
    pub conv_id: Option<String>,
    pub tx_signature: Option<String>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    In,
    Out,
    Internal,
}

impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
            Direction::Internal => "internal",
        }
    }

    pub fn parse(s: &str) -> Option<Direction> {
        match s {
            "in" => Some(Direction::In),
            "out" => Some(Direction::Out),
            "internal" => Some(Direction::Internal),
            _ => None,
        }
    }
}

/// One line tailed from a daemon's JSON tracing log.
#[derive(Debug, Clone)]
pub struct RawLogLine {
    pub source_file: PathBuf,
    pub raw: serde_json::Value,
}
