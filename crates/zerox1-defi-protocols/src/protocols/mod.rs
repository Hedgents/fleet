//! One module per protocol. Each exposes pure-Rust instruction builders.
//!
//! Conventions:
//! - Builders take a `user: &Pubkey` and amounts in raw token units (not UI).
//! - Builders return `Vec<Instruction>` so they can prepend ATA-create or
//!   compute-budget instructions when needed.
//! - Builders never sign and never broadcast.

pub mod adrena;
pub mod jito;
pub mod jito_loader;
pub mod jlp;
pub mod jupiter;
pub mod kamino;
pub mod kamino_loader;
pub mod pyth;
pub mod sanctum;
