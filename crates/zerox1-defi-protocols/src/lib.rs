// Pre-existing tech debt acked at crate level; tracked for cleanup separately.
#![allow(deprecated)] // solana_sdk::system_program → solana_system_interface migration
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::should_implement_trait)]
#![allow(clippy::too_many_arguments)]

//! Pure-Rust instruction builders for Solana DeFi protocols.
//!
//! This crate has zero runtime dependencies. It returns `solana_sdk::Instruction`
//! values that the caller is responsible for signing and broadcasting.
//!
//! Layout:
//! - [`constants`] — well-known program IDs, token mints, market PDAs
//! - [`error`]     — typed error enum
//! - [`util`]      — shared helpers (Anchor discriminators, ATA derivation)
//! - [`protocols`] — one module per protocol

pub mod constants;
pub mod error;
pub mod protocols;
pub mod util;

pub use error::Error;

pub type Result<T> = std::result::Result<T, Error>;
