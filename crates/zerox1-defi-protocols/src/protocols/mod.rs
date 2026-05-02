//! One module per protocol. Each exposes pure-Rust instruction builders.
//!
//! Conventions:
//! - Builders take a `user: &Pubkey` and amounts in raw token units (not UI).
//! - Builders return `Vec<Instruction>` so they can prepend ATA-create or
//!   compute-budget instructions when needed.
//! - Builders never sign and never broadcast.

pub mod kamino;
pub mod pyth;
