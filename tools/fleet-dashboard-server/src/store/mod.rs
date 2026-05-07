//! SQLite-backed store for the dashboard server.
//!
//! Concurrency model: rusqlite's `Connection` is sync, so we wrap it in a
//! `tokio::sync::Mutex` and run the handful of synchronous SQL calls under
//! that guard. Volume is low (~one insert per beacon/assign/report) and the
//! Day 1 ingest pipeline is single-writer, so contention is not a concern.
//! REST query handlers in Day 2 will read through the same mutex.

mod sqlite;

pub use sqlite::Store;
