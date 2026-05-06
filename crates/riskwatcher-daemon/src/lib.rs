//! Library surface for the riskwatcher-daemon.
//!
//! Modules here are exposed primarily for integration tests under
//! `tests/`. The binary entrypoint lives in `main.rs`.

pub mod observer;
pub mod poller;
pub mod state;
