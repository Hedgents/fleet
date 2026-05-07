//! Hedgents fleet-dashboard-server — local dashboard backend.
//!
//! - `ingest::log_tailer`: notify-based watcher that emits one
//!   `RawLogLine` per JSON-formatted tracing event.
//! - `ingest::envelope_decoder`: pure-fn decoder mapping a `RawLogLine`
//!   onto a human-readable `MeshEvent`.
//! - `ingest::pnl_jsonl`: tails `*-pnl.jsonl` and `researcher-signals.jsonl`,
//!   writes raw JSON to the `pnl_snapshots` table.
//! - `store`: SQLite store for `mesh_events` and `pnl_snapshots`.
//! - `chain`: best-effort on-chain reads (wallet balances, Kamino
//!   obligation, JLP balance) with 30s caching.
//! - `api`: axum REST + WebSocket router on 127.0.0.1:7700.

pub mod api;
pub mod chain;
pub mod ingest;
pub mod store;
pub mod types;

pub use store::Store;
pub use types::{Direction, MeshEvent, RawLogLine};
