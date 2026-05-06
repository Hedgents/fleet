//! Researcher daemon library surface — signal emission infrastructure.
//!
//! The binary lives in `main.rs` (boots the embedded node, runs the
//! beacon loop, owns the inbox). Modules here are pure logic that
//! watchers (M3+) and tests can pull in independently.

pub mod dedup;
pub mod signal;
pub mod telemetry;
pub mod thresholds;
pub mod watchers;
