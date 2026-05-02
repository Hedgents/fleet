//! Pyth Network price feed reader.
//!
//! ## Status: legacy decoder, awaiting Pull Oracle migration
//!
//! This module decodes Pyth's **legacy** price account schema (program
//! `FsJ3...`). As of mid-2025 most legacy Solana feeds are no longer
//! published — the accounts exist but their `status` field reads as 0
//! (Unknown), and our decoder correctly refuses to return stale prices.
//!
//! This is honest behavior: a Risk Watcher should never receive a stale
//! price as if it were live. The price/conf/expo extraction logic is
//! correct and unit-tested; what's stale is the upstream feed source.
//!
//! ## Migration path (v0.2.0)
//!
//! Switch to Pyth's Pull Oracle (Wormhole receiver, program
//! `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`). The Pull model receives
//! signed price updates from Wormhole; current pricing comes via
//! `pyth-solana-receiver-sdk`. The shape of `PythPrice` does not change;
//! only the fetch + decode path changes.
//!
//! Until that lands, this module is useful for:
//!   - Local unit testing of risk math (synthetic price construction)
//!   - Decoding any legacy feeds that may still be active for niche assets
//!   - Reference for the `PythPrice` shape that Pull Oracle handlers will
//!     eventually return
//!
//! Reference: Pyth's published price-account schema (offset 0..240 bytes
//! contain magic, version, account_type, size, ptype, expo, num_publishers
//! plus the `agg` PriceInfo at offset 208). We pull `agg.price` (i64),
//! `agg.conf` (u64), `expo` (i32), and `pub_slot` (u64).

use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PythError {
    #[error("price account too small: {0} bytes")]
    AccountTooSmall(usize),
    #[error("magic mismatch (got 0x{0:08x}, expected 0xa1b2c3d4)")]
    BadMagic(u32),
    #[error("price status not Trading (status code {0})")]
    NotTrading(u32),
}

/// Pyth price-account "magic" — first 4 bytes of every Pyth account.
const PYTH_MAGIC: u32 = 0xa1b2_c3d4;

/// Minimum account size for the legacy price account schema.
/// (Real accounts are larger; we just need enough to reach `agg`.)
const MIN_PRICE_ACCOUNT_LEN: usize = 240;

// ── Byte offsets (from Pyth's published price-account schema) ───────────────
// Header:
//   0..4   magic        u32
//   4..8   ver          u32
//   8..12  account_type u32   (3 = price)
//   12..16 size         u32
//   16..20 ptype        u32   (1 = price)
//   20..24 expo         i32   (negative = decimals shift)
//
// agg PriceInfo lives at offset 208 with this layout:
//   208..216 price        i64
//   216..224 conf         u64
//   224..228 status       u32   (1 = Trading)
//   228..232 corp_act     u32
//   232..240 pub_slot     u64
const OFF_MAGIC: usize       = 0;
const OFF_EXPO: usize        = 20;
const OFF_AGG_PRICE: usize   = 208;
const OFF_AGG_CONF: usize    = 216;
const OFF_AGG_STATUS: usize  = 224;
const OFF_AGG_PUBSLOT: usize = 232;

/// Decoded price information.
#[derive(Debug, Clone, PartialEq)]
pub struct PythPrice {
    /// The raw price (multiply by 10^expo to get the true value).
    pub price: i64,
    /// Confidence interval in the same units as `price`.
    pub conf: u64,
    /// Decimal exponent. e.g. expo = -8 → divide by 1e8.
    pub expo: i32,
    /// Slot the price was published at.
    pub pub_slot: u64,
}

impl PythPrice {
    /// Convert to a floating-point price for human display. Loses precision;
    /// risk math should stay in (price, expo) integer form.
    pub fn as_f64(&self) -> f64 {
        (self.price as f64) * 10f64.powi(self.expo)
    }

    /// Confidence interval as a fraction of price (basis points).
    pub fn conf_bps(&self) -> u32 {
        if self.price <= 0 {
            return u32::MAX;
        }
        let frac = (self.conf as f64) / (self.price as f64);
        (frac * 10_000.0).round().min(u32::MAX as f64) as u32
    }
}

/// Decode a Pyth legacy price account. Verifies magic and Trading status.
pub fn decode_price(data: &[u8]) -> Result<PythPrice, PythError> {
    if data.len() < MIN_PRICE_ACCOUNT_LEN {
        return Err(PythError::AccountTooSmall(data.len()));
    }

    let magic = read_u32_le(data, OFF_MAGIC);
    if magic != PYTH_MAGIC {
        return Err(PythError::BadMagic(magic));
    }

    let status = read_u32_le(data, OFF_AGG_STATUS);
    // Status: 0=Unknown, 1=Trading, 2=Halted, 3=Auction, 4=Ignored
    if status != 1 {
        return Err(PythError::NotTrading(status));
    }

    let expo     = read_i32_le(data, OFF_EXPO);
    let price    = read_i64_le(data, OFF_AGG_PRICE);
    let conf     = read_u64_le(data, OFF_AGG_CONF);
    let pub_slot = read_u64_le(data, OFF_AGG_PUBSLOT);

    Ok(PythPrice { price, conf, expo, pub_slot })
}

/// Resolve a symbol to its mainnet/devnet Pyth price feed account.
///
/// Pyth uses different feed addresses per network. For the Risk Watcher we
/// only need a small set: SOL, USDC, JITOSOL, INF, ETH, BTC.
pub fn feed_for_symbol(symbol: &str, devnet: bool) -> Option<Pubkey> {
    use solana_sdk::pubkey;
    let s = symbol.to_ascii_uppercase();
    if devnet {
        match s.as_str() {
            "SOL"  => Some(pubkey!("J83w4HKfqxwcq3BEMMkPFSppX3gqekLyLJBexebFVkix")),
            "USDC" => Some(pubkey!("5SSkXsEKQepHHAewytPVwdej4epN1nxgLVM84L4KXgy7")),
            "BTC"  => Some(pubkey!("HovQMDrbAgAYPCmHVSrezcSmkMtXSSUsLDFANExrZh2J")),
            "ETH"  => Some(pubkey!("EdVCmQ9FSPcVRNYbWwzJTcFHQVF6JKkebS6vWdBoKzkP")),
            _ => None,
        }
    } else {
        match s.as_str() {
            "SOL"     => Some(pubkey!("H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4AQJEG")),
            "USDC"    => Some(pubkey!("Gnt27xtC473ZT2Mw5u8wZ68Z3gULkSTb5DuxJy7eJotD")),
            "JITOSOL" => Some(pubkey!("7yyaeuJ1GGtVBLT2z2xub5ZWYKaNhF28mj1RdV4VDFVk")),
            "BTC"     => Some(pubkey!("GVXRSBjFk6e6J3NbVPXohDJetcTjaeeuykUpbQF8UoMU")),
            "ETH"     => Some(pubkey!("JBu1AL4obBcCMqKBBxhpWCNUt136ijcuMZLFvTP7iWdB")),
            _ => None,
        }
    }
}

// ── Little-endian readers ───────────────────────────────────────────────────

fn read_u32_le(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().expect("bounds checked"))
}
fn read_i32_le(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(data[off..off + 4].try_into().expect("bounds checked"))
}
fn read_i64_le(data: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(data[off..off + 8].try_into().expect("bounds checked"))
}
fn read_u64_le(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().expect("bounds checked"))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic price account with given fields.
    fn build_account(
        magic: u32,
        expo: i32,
        price: i64,
        conf: u64,
        status: u32,
        pub_slot: u64,
    ) -> Vec<u8> {
        let mut data = vec![0u8; MIN_PRICE_ACCOUNT_LEN];
        data[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&magic.to_le_bytes());
        data[OFF_EXPO..OFF_EXPO + 4].copy_from_slice(&expo.to_le_bytes());
        data[OFF_AGG_PRICE..OFF_AGG_PRICE + 8].copy_from_slice(&price.to_le_bytes());
        data[OFF_AGG_CONF..OFF_AGG_CONF + 8].copy_from_slice(&conf.to_le_bytes());
        data[OFF_AGG_STATUS..OFF_AGG_STATUS + 4].copy_from_slice(&status.to_le_bytes());
        data[OFF_AGG_PUBSLOT..OFF_AGG_PUBSLOT + 8].copy_from_slice(&pub_slot.to_le_bytes());
        data
    }

    #[test]
    fn decodes_well_formed_trading_price() {
        let data = build_account(PYTH_MAGIC, -8, 12_345_678_900, 50_000, 1, 99_999);
        let p = decode_price(&data).unwrap();
        assert_eq!(p.price, 12_345_678_900);
        assert_eq!(p.conf, 50_000);
        assert_eq!(p.expo, -8);
        assert_eq!(p.pub_slot, 99_999);
    }

    #[test]
    fn rejects_bad_magic() {
        let data = build_account(0xdeadbeef, -8, 1, 0, 1, 0);
        assert!(matches!(decode_price(&data), Err(PythError::BadMagic(_))));
    }

    #[test]
    fn rejects_too_small_account() {
        let data = vec![0u8; 50];
        assert!(matches!(decode_price(&data), Err(PythError::AccountTooSmall(50))));
    }

    #[test]
    fn rejects_halted_status() {
        let data = build_account(PYTH_MAGIC, -8, 1, 0, 2, 0);
        assert!(matches!(decode_price(&data), Err(PythError::NotTrading(2))));
    }

    #[test]
    fn rejects_unknown_status() {
        let data = build_account(PYTH_MAGIC, -8, 1, 0, 0, 0);
        assert!(matches!(decode_price(&data), Err(PythError::NotTrading(0))));
    }

    #[test]
    fn as_f64_applies_negative_expo() {
        let p = PythPrice { price: 12_345_678_900, conf: 0, expo: -8, pub_slot: 0 };
        // 12_345_678_900 * 1e-8 = 123.456789
        assert!((p.as_f64() - 123.456789).abs() < 1e-6);
    }

    #[test]
    fn conf_bps_for_normal_price() {
        let p = PythPrice { price: 100_000_000, conf: 100_000, expo: -8, pub_slot: 0 };
        // conf is 0.1% of price → 10 bps
        assert_eq!(p.conf_bps(), 10);
    }

    #[test]
    fn conf_bps_for_zero_price_saturates() {
        let p = PythPrice { price: 0, conf: 1000, expo: -8, pub_slot: 0 };
        assert_eq!(p.conf_bps(), u32::MAX);
    }

    #[test]
    fn feed_lookup_mainnet_known_symbols() {
        assert!(feed_for_symbol("SOL", false).is_some());
        assert!(feed_for_symbol("usdc", false).is_some());  // case-insensitive
        assert!(feed_for_symbol("JITOSOL", false).is_some());
        assert!(feed_for_symbol("XYZ", false).is_none());
    }

    #[test]
    fn feed_lookup_devnet_subset() {
        assert!(feed_for_symbol("SOL", true).is_some());
        assert!(feed_for_symbol("JITOSOL", true).is_none(), "JITOSOL is mainnet-only");
    }
}
