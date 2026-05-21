//! Library face of `fleet-pm-stub`.
//!
//! The CLI binary stays the primary consumer, but the orchestrator daemon
//! (`crates/orchestrator-daemon`) reuses the pure allocator decision
//! function and the snapshot-fetching glue, so both modules are exposed
//! as a library crate.
//!
//! Everything else in the binary (envelope assembly, NodeService boot,
//! subcommand dispatch) lives only in `main.rs` and is not part of the
//! library surface.

pub mod allocator;
pub mod allocator_apr_weighted;
pub mod allocator_runner;
pub mod allocator_targets;
