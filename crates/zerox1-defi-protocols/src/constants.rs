//! Well-known Solana program IDs, token mints, and market PDAs.
//!
//! All addresses are mainnet unless noted. Devnet equivalents are kept in
//! a parallel module (`devnet`) where they exist.

use solana_sdk::{pubkey, pubkey::Pubkey};

// ── Solana system / SPL ─────────────────────────────────────────────────────

pub const SYSTEM_PROGRAM_ID: Pubkey = pubkey!("11111111111111111111111111111111");
pub const TOKEN_PROGRAM_ID: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey =
    pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSVAR_INSTRUCTIONS_ID: Pubkey = pubkey!("Sysvar1nstructions1111111111111111111111111");
pub const SYSVAR_RENT_ID: Pubkey = pubkey!("SysvarRent111111111111111111111111111111111");

// ── Token mints ─────────────────────────────────────────────────────────────

pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const USDC_MINT: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDT_MINT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");

// LSTs
pub const JITOSOL_MINT: Pubkey = pubkey!("J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn");

// ── SPL Stake Pool (Jito) ───────────────────────────────────────────────────
//
// Shared SPL stake-pool program; Jito's pool is one of many instances.
pub const SPL_STAKE_POOL_PROGRAM_ID: Pubkey =
    pubkey!("SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy");

/// Jito Stake Pool — the only stake pool we touch from this fleet.
/// Mints jitoSOL when deposited to.
pub const JITO_STAKE_POOL: Pubkey = pubkey!("Jito4APyf642JPZPx3hGc6WWJ8zPKtRbRs4P815Awbb");
pub const INF_MINT: Pubkey = pubkey!("5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm");
pub const BSOL_MINT: Pubkey = pubkey!("bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1");
pub const MSOL_MINT: Pubkey = pubkey!("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So");

// ── Kamino lending ──────────────────────────────────────────────────────────

/// Kamino Lend (klend) program ID — mainnet.
pub const KAMINO_LEND_PROGRAM_ID: Pubkey = pubkey!("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD");

/// Kamino main market lending market account.
/// This is the primary lending market with USDC, SOL, jitoSOL etc. listed.
pub const KAMINO_MAIN_MARKET: Pubkey = pubkey!("7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF");

/// Kamino main market USDC reserve.
pub const KAMINO_MAIN_USDC_RESERVE: Pubkey =
    pubkey!("D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59");

/// Kamino Farms program ID — mainnet.
/// Required for RefreshObligationFarmsForReserve when a reserve has farms.
pub const KAMINO_FARMS_PROGRAM_ID: Pubkey = pubkey!("FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr");

/// Kamino main market SOL reserve.
pub const KAMINO_MAIN_SOL_RESERVE: Pubkey = pubkey!("d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q");

/// Kamino main market jitoSOL reserve.
pub const KAMINO_MAIN_JITOSOL_RESERVE: Pubkey =
    pubkey!("EVbyPKrHG6WBfm4dLxLMJpUDY43cCAcHSpV3KYjKsktW");

// ── Jupiter Perpetuals (JLP) ────────────────────────────────────────────────

pub const JUPITER_PERPETUALS_PROGRAM_ID: Pubkey =
    pubkey!("PERPHjGBqRHArX4DySjwM6UJHiR3sWAatqfdBS2qQJu");

pub const JLP_MINT: Pubkey = pubkey!("27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4");

/// The single JLP pool account — there is only one main pool ("Pool") on
/// Jupiter Perps. Holds the 5 custodies, AUM, and fee config.
pub const JLP_POOL: Pubkey = pubkey!("5BUwFW4nRbftYTDMbgxykoFWqWHPzahFSNAaaaJtVKsq");

/// JLP pool's two non-stable, non-SOL underlying assets — Wormhole portal
/// wrapped versions of ETH and BTC, used by Jupiter Perps.
pub const WETH_PORTAL_MINT: Pubkey = pubkey!("7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs");
pub const WBTC_PORTAL_MINT: Pubkey = pubkey!("3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh");

// ── Adrena ──────────────────────────────────────────────────────────────────
//
// Mainnet addresses verified against on-chain account scans on 2026-05-04.
// Adrena exposes a single global "main-pool" with 4 active custodies indexed
// 0..4: USDC, BONK, JitoSOL, WBTC.

pub const ADRENA_PROGRAM_ID: Pubkey = pubkey!("13gDzEXCdocbj8iAiqrScGo47NiSuYENGsRqi3SEAwet");

/// Adrena "main-pool" — the only active liquidity pool.
pub const ADRENA_MAIN_POOL: Pubkey = pubkey!("4bQRutgDJs6vuh6ZcWaPVXiQaBzbHketjbCDjL4oRN34");

/// Pool custody indices (positional in pool.custodies array).
pub const ADRENA_CUSTODY_USDC: Pubkey = pubkey!("Dk523LZeDQbZtUwPEBjFXCd2Au1tD7mWZBJJmcgHktNk");
pub const ADRENA_CUSTODY_BONK: Pubkey = pubkey!("8aJuzsgjxBnvRhDcfQBD7z4CUj7QoPEpaNwVd7KqsSk5");
/// Adrena's "SOL" custody is actually JitoSOL — used for any SOL-direction
/// positions including hedge shorts.
pub const ADRENA_CUSTODY_JITOSOL: Pubkey = pubkey!("GZ9XfWwgTRhkma2Y91Q9r1XKotNXYjBnKKabj19rhT71");
pub const ADRENA_CUSTODY_WBTC: Pubkey = pubkey!("GFu3qS22mo6bAjg4Lr5R7L8pPgHq6GvbjJPKEHkbbs2c");

// ── Sanctum INF ─────────────────────────────────────────────────────────────
// Router endpoint is HTTP-based; no on-chain program ID needed for stake/unstake
// (Sanctum router builds a Jupiter-style versioned tx server-side).

// ── Pyth ────────────────────────────────────────────────────────────────────

pub const PYTH_PROGRAM_ID: Pubkey = pubkey!("FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH");

pub const PYTH_SOL_USD_FEED: Pubkey = pubkey!("H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4AQJEG");
pub const PYTH_USDC_USD_FEED: Pubkey = pubkey!("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD");
pub const PYTH_JITOSOL_USD_FEED: Pubkey = pubkey!("7yyaeuJ1GGtVBLT2z2xub5ZWYKaNhF28mj1RdV4VDFVk");
