//! Hedgents fleet-dashboard-server — local dashboard backend.
//!
//! Day 1 scope: ingest pipeline only.
//! - `ingest::log_tailer`: notify-based watcher that emits one
//!   `RawLogLine` per JSON-formatted tracing event.
//! - `ingest::envelope_decoder`: pure-fn decoder mapping a `RawLogLine`
//!   onto a human-readable `MeshEvent`.
//! - `ingest::pnl_jsonl`: tails `*-pnl.jsonl` and `researcher-signals.jsonl`,
//!   writes raw JSON to the `pnl_snapshots` table.
//! - `store`: SQLite store for `mesh_events` and `pnl_snapshots`.
//!
//! REST + WebSocket API and chain reads land Day 2.

pub mod ingest;
pub mod store;
pub mod types;

pub use store::Store;
pub use types::{Direction, MeshEvent, RawLogLine};
