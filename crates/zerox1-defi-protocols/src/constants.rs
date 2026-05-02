//! Well-known Solana program IDs, token mints, and market PDAs.
//!
//! All addresses are mainnet unless noted. Devnet equivalents are kept in
//! a parallel module (`devnet`) where they exist.

use solana_sdk::{pubkey, pubkey::Pubkey};

// ── Solana system / SPL ─────────────────────────────────────────────────────

pub const SYSTEM_PROGRAM_ID: Pubkey = pubkey!("11111111111111111111111111111111");
pub const TOKEN_PROGRAM_ID: Pubkey =
    pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSVAR_INSTRUCTIONS_ID: Pubkey =
    pubkey!("Sysvar1nstructions1111111111111111111111111");
pub const SYSVAR_RENT_ID: Pubkey =
    pubkey!("SysvarRent111111111111111111111111111111111");

// ── Token mints ─────────────────────────────────────────────────────────────

pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDT_MINT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");

// LSTs
pub const JITOSOL_MINT: Pubkey =
    pubkey!("J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn");
pub const INF_MINT: Pubkey =
    pubkey!("5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm");
pub const BSOL_MINT: Pubkey =
    pubkey!("bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1");
pub const MSOL_MINT: Pubkey =
    pubkey!("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So");

// ── Kamino lending ──────────────────────────────────────────────────────────

/// Kamino Lend (klend) program ID — mainnet.
pub const KAMINO_LEND_PROGRAM_ID: Pubkey =
    pubkey!("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD");

/// Kamino main market lending market account.
/// This is the primary lending market with USDC, SOL, jitoSOL etc. listed.
pub const KAMINO_MAIN_MARKET: Pubkey =
    pubkey!("7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF");

/// Kamino main market USDC reserve.
pub const KAMINO_MAIN_USDC_RESERVE: Pubkey =
    pubkey!("D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59");

/// Kamino main market SOL reserve.
pub const KAMINO_MAIN_SOL_RESERVE: Pubkey =
    pubkey!("d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q");

/// Kamino main market jitoSOL reserve.
pub const KAMINO_MAIN_JITOSOL_RESERVE: Pubkey =
    pubkey!("EVbyPKrHG6WBfm4dLxLMJpUDY43cCAcHSpV3KYjKsktW");

// ── Jupiter Perpetuals (JLP) ────────────────────────────────────────────────

pub const JUPITER_PERPETUALS_PROGRAM_ID: Pubkey =
    pubkey!("PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu");

pub const JLP_MINT: Pubkey = pubkey!("27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4");

// ── Adrena ──────────────────────────────────────────────────────────────────
// Program ID and pool accounts to be filled in next iteration.
// Adrena IDL: https://github.com/AdrenaFinance/perpetuals

// ── Sanctum INF ─────────────────────────────────────────────────────────────
// Router endpoint is HTTP-based; no on-chain program ID needed for stake/unstake
// (Sanctum router builds a Jupiter-style versioned tx server-side).

// ── Pyth ────────────────────────────────────────────────────────────────────

pub const PYTH_PROGRAM_ID: Pubkey =
    pubkey!("FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH");

pub const PYTH_SOL_USD_FEED: Pubkey =
    pubkey!("H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4AQJEG");
pub const PYTH_USDC_USD_FEED: Pubkey =
    pubkey!("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD");
pub const PYTH_JITOSOL_USD_FEED: Pubkey =
    pubkey!("7yyaeuJ1GGtVBLT2z2xub5ZWYKaNhF28mj1RdV4VDFVk");
