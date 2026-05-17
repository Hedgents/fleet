//! Orchestrator daemon — autonomous regime-aware allocator.
//!
//! Polls the dashboard REST API on a tick, runs the pure
//! [`fleet_pm_stub::allocator::decide`] function against the live
//! snapshot, and writes each decision to an append-only JSONL audit
//! log. With `--execute`, signs and dispatches `Assign`/`Withdraw`
//! envelopes through the embedded `NodeService` to the strategy
//! daemons, gated by per-strategy cooldowns and a stale-snapshot
//! guard.
//!
//! The orchestrator never signs Solana transactions directly — the
//! `zerox1-defi-wallet` crate is not in its dependency graph. It
//! emits *mesh envelopes* that the role-bound strategy daemons sign
//! and execute through their existing approval queues.
//!
//! See [`ROADMAP.md`](../../../ROADMAP.md) Phase 1 for context.

pub mod cooldown;
pub mod emit;
pub mod telemetry;
pub mod tick;
