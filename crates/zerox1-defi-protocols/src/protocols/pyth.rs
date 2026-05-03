//! Pyth Pull Oracle (Wormhole receiver) price reader.
//!
//! Decodes `PriceUpdateV2` accounts owned by the Pyth Solana Receiver
//! program (`rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`).
//!
//! ## Why not the legacy `FsJ3...` decoder
//!
//! Pyth deprecated the legacy Solana price-account schema in mid-2025.
//! Those accounts still exist on chain but their `status` field reads as 0
//! (Unknown) — Pyth's keepers no longer publish to them. The current data
//! flow is:
//!
//!   Pythnet (price aggregation)
//!     → Wormhole (signed VAA)
//!       → Pyth Solana Receiver (verifies, writes to a PriceUpdateV2 account)
//!
//! Pyth maintains "sponsored" feed accounts at deterministic PDAs derived
//! from a feed_id; their bots keep these fresh. We read those.
//!
//! ## Sponsored feed account addresses
//!
//! Sponsored feed accounts are NOT PDAs — they are regular accounts that
//! Pyth's keeper bots create with their own keypair, then post updates to.
//! The address per asset is whichever account the keeper picked. We
//! hardcode the verified mainnet sponsored addresses below; new assets
//! are added by querying `getProgramAccounts` on the receiver with a
//! memcmp filter on `feed_id` at offset 41 and picking the
//! most-recently-updated result (highest `posted_slot`).
//!
//! ## PriceUpdateV2 account layout (134 bytes for FullyVerified)
//!
//! ```text
//! offset  size  field
//! ────── ──── ─────────────────
//!     0    8   discriminator (Anchor: sha256("account:PriceUpdateV2")[..8]
//!              = 22 f1 23 63 9d 7e f4 cd)
//!     8   32   write_authority (Pubkey)
//!    40    1   verification_level tag
//!                0 = PartiallyVerified { num_signatures: u8 } → +1 byte
//!                1 = FullyVerified                            → +0 bytes
//!    +0   32   price_message.feed_id ([u8; 32])
//!    +32   8   price_message.price (i64)
//!    +40   8   price_message.conf (u64)
//!    +48   4   price_message.exponent (i32)
//!    +52   8   price_message.publish_time (i64, unix seconds)
//!    +60   8   price_message.prev_publish_time (i64)
//!    +68   8   price_message.ema_price (i64)
//!    +76   8   price_message.ema_conf (u64)
//!    +84   8   posted_slot (u64)
//!    +92   1   trailing pad (Anchor)
//! ```
//!
//! Verified live against the SOL/USD sponsored feed on mainnet
//! (`7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE`).

use solana_sdk::pubkey::Pubkey;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PythError {
    #[error("price account too small: {0} bytes (need at least 133 for FullyVerified)")]
    AccountTooSmall(usize),
    #[error("discriminator mismatch (got {got_hex}, expected 22f123639d7ef4cd for PriceUpdateV2)")]
    BadDiscriminator { got_hex: String },
    #[error("unknown verification_level tag: {0}")]
    BadVerificationLevel(u8),
    #[error("price feed_id mismatch: account has {actual_hex}, expected {expected_hex}")]
    FeedIdMismatch { actual_hex: String, expected_hex: String },
}

/// Pyth Solana Receiver program (mainnet + devnet).
pub const PYTH_RECEIVER_PROGRAM_ID: Pubkey =
    solana_sdk::pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");

/// PriceUpdateV2 account discriminator: sha256("account:PriceUpdateV2")[..8].
pub const PRICE_UPDATE_V2_DISCRIMINATOR: [u8; 8] =
    [0x22, 0xf1, 0x23, 0x63, 0x9d, 0x7e, 0xf4, 0xcd];

// ── Verification level tags ─────────────────────────────────────────────────
const VL_PARTIALLY_VERIFIED: u8 = 0;
const VL_FULLY_VERIFIED: u8 = 1;

// ── Field offsets within the price_message sub-struct ──────────────────────
// (offsets are RELATIVE to the start of the price_message)
const PM_FEED_ID: usize           = 0;
const PM_PRICE: usize             = 32;
const PM_CONF: usize              = 40;
const PM_EXPO: usize              = 48;
const PM_PUBLISH_TIME: usize      = 52;
const PM_PREV_PUBLISH_TIME: usize = 60;
const PM_EMA_PRICE: usize         = 68;
const PM_EMA_CONF: usize          = 76;
const PRICE_MESSAGE_SIZE: usize   = 84;

/// Decoded price information.
#[derive(Debug, Clone, PartialEq)]
pub struct PythPrice {
    /// Pyth feed identifier (32 bytes; same on every chain Pyth publishes to).
    pub feed_id: [u8; 32],
    /// Spot price (multiply by 10^expo to get the value).
    pub price: i64,
    /// Confidence interval in the same units as `price`.
    pub conf: u64,
    /// Decimal exponent. e.g. expo = -8 → divide by 1e8.
    pub expo: i32,
    /// Wall-clock unix timestamp Pyth published this update.
    pub publish_time: i64,
    /// Previous publish_time, useful for cadence checks.
    pub prev_publish_time: i64,
    /// EMA price (smoothed; useful for cross-checking against spike attacks).
    pub ema_price: i64,
    /// EMA confidence interval.
    pub ema_conf: u64,
    /// Solana slot the receiver wrote this update at.
    pub posted_slot: u64,
}

impl PythPrice {
    /// Convert spot price to f64 for human display. Loses precision; risk
    /// math should stay in (price, expo) integer form.
    pub fn as_f64(&self) -> f64 {
        (self.price as f64) * 10f64.powi(self.expo)
    }

    /// EMA price as f64.
    pub fn ema_as_f64(&self) -> f64 {
        (self.ema_price as f64) * 10f64.powi(self.expo)
    }

    /// Confidence interval as basis points of price.
    pub fn conf_bps(&self) -> u32 {
        if self.price <= 0 {
            return u32::MAX;
        }
        let frac = (self.conf as f64) / (self.price as f64);
        (frac * 10_000.0).round().min(u32::MAX as f64) as u32
    }

    /// Seconds since this price was published, from caller-supplied `now`.
    /// Use `chrono::Utc::now().timestamp()` or similar at the call site.
    /// Returns 0 if the publish_time is in the future (clock skew).
    pub fn age_seconds(&self, now_unix_seconds: i64) -> u64 {
        (now_unix_seconds.saturating_sub(self.publish_time)).max(0) as u64
    }
}

/// Decode a `PriceUpdateV2` account. Verifies discriminator and parses
/// price + EMA fields. Does NOT verify the `write_authority` — caller
/// should match against the expected sponsored authority if it cares.
pub fn decode_price(data: &[u8]) -> Result<PythPrice, PythError> {
    // Header: 8 (discriminator) + 32 (write_authority) + 1 (verification_level tag)
    if data.len() < 41 {
        return Err(PythError::AccountTooSmall(data.len()));
    }

    if data[..8] != PRICE_UPDATE_V2_DISCRIMINATOR {
        return Err(PythError::BadDiscriminator {
            got_hex: hex::encode(&data[..8]),
        });
    }

    // Skip write_authority (8..40), parse verification_level tag at 40.
    let vl_tag = data[40];
    let pm_start = match vl_tag {
        VL_FULLY_VERIFIED      => 41,
        VL_PARTIALLY_VERIFIED  => 42,  // +1 byte for num_signatures
        other => return Err(PythError::BadVerificationLevel(other)),
    };

    // price_message + posted_slot
    let pm_end = pm_start + PRICE_MESSAGE_SIZE;
    let posted_slot_end = pm_end + 8;
    if data.len() < posted_slot_end {
        return Err(PythError::AccountTooSmall(data.len()));
    }

    let pm = &data[pm_start..pm_end];
    let mut feed_id = [0u8; 32];
    feed_id.copy_from_slice(&pm[PM_FEED_ID..PM_FEED_ID + 32]);

    let price             = read_i64_le(pm, PM_PRICE);
    let conf              = read_u64_le(pm, PM_CONF);
    let expo              = read_i32_le(pm, PM_EXPO);
    let publish_time      = read_i64_le(pm, PM_PUBLISH_TIME);
    let prev_publish_time = read_i64_le(pm, PM_PREV_PUBLISH_TIME);
    let ema_price         = read_i64_le(pm, PM_EMA_PRICE);
    let ema_conf          = read_u64_le(pm, PM_EMA_CONF);
    let posted_slot       = read_u64_le(data, pm_end);

    Ok(PythPrice {
        feed_id, price, conf, expo,
        publish_time, prev_publish_time,
        ema_price, ema_conf, posted_slot,
    })
}

/// Resolve a symbol to its sponsored Pull feed account.
///
/// Both networks use the same Pyth Receiver program. Sponsored feed
/// addresses below are mainnet — Pyth maintains separate keepers and
/// addresses on devnet. We support mainnet only for now; devnet support
/// requires a separate lookup table.
///
/// Verified live on 2026-05-03 against `getProgramAccounts` filtered by
/// feed_id, picking the highest `posted_slot` candidate.
pub fn feed_for_symbol(symbol: &str, devnet: bool) -> Option<Pubkey> {
    if devnet {
        // Pyth Pull on devnet uses a separate keeper set; addresses differ.
        // Tracked as v0.2.0 follow-up. Returning None here (rather than
        // mainnet addresses) avoids silently giving the wrong account.
        return None;
    }
    use solana_sdk::pubkey;
    match symbol.to_ascii_uppercase().as_str() {
        "SOL"     => Some(pubkey!("7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE")),
        "USDC"    => Some(pubkey!("Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX")),
        "USDT"    => Some(pubkey!("HT2PLQBcG5EiCcNSaMHAjSgd9F98ecpATbk4Sk5oYuM")),
        "BTC"     => Some(pubkey!("4cSM2e6rvbGQUFiJbqytoVMi5GgghSMr8LwVrT9VPSPo")),
        "ETH"     => Some(pubkey!("42amVS4KgzR9rA28tkVYqVXjq9Qa8dcZQMbH5EYFX6XC")),
        "JITOSOL" => Some(pubkey!("AxaxyeDT8JnWERSaTKvFXvPKkEdxnamKSqpWbsSjYg1g")),
        "INF"     => Some(pubkey!("Ceg5oePJv1a6RR541qKeQaTepvERA3i8SvyueX9tT8Sq")),
        "BSOL"    => Some(pubkey!("5cN76Xm2Dtx9MnrQqBDeZZRsWruTTcw37UruznAdSvvE")),
        _ => None,
    }
}

/// Map a symbol to its 32-byte canonical Pyth feed_id.
/// Useful for verifying that a fetched account matches the symbol the caller asked for.
pub fn feed_id_for_symbol(symbol: &str) -> Option<[u8; 32]> {
    let hex = match symbol.to_ascii_uppercase().as_str() {
        "SOL"     => "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
        "USDC"    => "eaa020c61cc479712813461ce153894a96a6c00b21ed0cfc2798d1f9a9e9c94a",
        "USDT"    => "2b89b9dc8fdf9f34709a5b106b472f0f39bb6ca9ce04b0fd7f2e971688e2e53b",
        "BTC"     => "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
        "ETH"     => "ff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
        "JITOSOL" => "67be9f519b95cf24338801051f9a808eff0a578ccb388db73b7f6fe1de019ffb",
        "INF"     => "f51570985c642c49c2d6e50156390fdba80bb6d5f7fa389d2f012ced4f7d208f",
        "BSOL"    => "89875379e70f8fbadc17aef315adf3a8d5d160b811435537e03c97e8aac97d9c",
        _ => return None,
    };
    let mut out = [0u8; 32];
    out.copy_from_slice(&hex::decode(hex).expect("constant"));
    Some(out)
}

// ── Little-endian readers ───────────────────────────────────────────────────

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

    /// Build a synthetic FullyVerified PriceUpdateV2 account.
    /// Mirrors the byte layout we live-verified against the mainnet SOL feed.
    fn build_account(
        discriminator: [u8; 8],
        verification_level: u8,
        feed_id: [u8; 32],
        price: i64,
        conf: u64,
        expo: i32,
        publish_time: i64,
        prev_publish_time: i64,
        ema_price: i64,
        ema_conf: u64,
        posted_slot: u64,
    ) -> Vec<u8> {
        let mut data = Vec::with_capacity(134);
        data.extend_from_slice(&discriminator);
        data.extend_from_slice(&[0u8; 32]);  // write_authority
        data.push(verification_level);
        if verification_level == VL_PARTIALLY_VERIFIED {
            data.push(0);  // num_signatures
        }
        data.extend_from_slice(&feed_id);
        data.extend_from_slice(&price.to_le_bytes());
        data.extend_from_slice(&conf.to_le_bytes());
        data.extend_from_slice(&expo.to_le_bytes());
        data.extend_from_slice(&publish_time.to_le_bytes());
        data.extend_from_slice(&prev_publish_time.to_le_bytes());
        data.extend_from_slice(&ema_price.to_le_bytes());
        data.extend_from_slice(&ema_conf.to_le_bytes());
        data.extend_from_slice(&posted_slot.to_le_bytes());
        data.push(0);  // trailing pad observed on real accounts
        data
    }

    fn sol_feed_id() -> [u8; 32] {
        feed_id_for_symbol("SOL").expect("SOL feed_id")
    }

    #[test]
    fn decodes_well_formed_fully_verified_account() {
        let data = build_account(
            PRICE_UPDATE_V2_DISCRIMINATOR, VL_FULLY_VERIFIED, sol_feed_id(),
            8_435_373_271, 3_901_413, -8, 1_777_763_642, 1_777_763_641,
            8_400_000_000, 4_000_000, 417_192_650,
        );
        let p = decode_price(&data).expect("decode");
        assert_eq!(p.feed_id, sol_feed_id());
        assert_eq!(p.price, 8_435_373_271);
        assert_eq!(p.conf, 3_901_413);
        assert_eq!(p.expo, -8);
        assert_eq!(p.publish_time, 1_777_763_642);
        assert_eq!(p.posted_slot, 417_192_650);
    }

    #[test]
    fn decodes_partially_verified_account_with_offset_shift() {
        let data = build_account(
            PRICE_UPDATE_V2_DISCRIMINATOR, VL_PARTIALLY_VERIFIED, sol_feed_id(),
            100, 1, -2, 0, 0, 100, 1, 0,
        );
        let p = decode_price(&data).expect("decode partially-verified");
        assert_eq!(p.price, 100);
        assert_eq!(p.expo, -2);
    }

    #[test]
    fn rejects_bad_discriminator() {
        let data = build_account(
            [0xde; 8], VL_FULLY_VERIFIED, sol_feed_id(),
            0, 0, 0, 0, 0, 0, 0, 0,
        );
        assert!(matches!(decode_price(&data), Err(PythError::BadDiscriminator { .. })));
    }

    #[test]
    fn rejects_unknown_verification_level() {
        let mut data = build_account(
            PRICE_UPDATE_V2_DISCRIMINATOR, VL_FULLY_VERIFIED, sol_feed_id(),
            0, 0, 0, 0, 0, 0, 0, 0,
        );
        data[40] = 99;  // invalid tag
        assert!(matches!(decode_price(&data), Err(PythError::BadVerificationLevel(99))));
    }

    #[test]
    fn rejects_truncated_account() {
        let data = vec![0u8; 30];
        assert!(matches!(decode_price(&data), Err(PythError::AccountTooSmall(30))));
    }

    #[test]
    fn as_f64_applies_negative_expo() {
        let p = PythPrice {
            feed_id: [0; 32], price: 8_435_373_271, conf: 0, expo: -8,
            publish_time: 0, prev_publish_time: 0, ema_price: 0, ema_conf: 0, posted_slot: 0,
        };
        assert!((p.as_f64() - 84.35373271).abs() < 1e-6);
    }

    #[test]
    fn conf_bps_for_normal_price() {
        let p = PythPrice {
            feed_id: [0; 32], price: 100_000_000, conf: 100_000, expo: -8,
            publish_time: 0, prev_publish_time: 0, ema_price: 0, ema_conf: 0, posted_slot: 0,
        };
        assert_eq!(p.conf_bps(), 10);  // 0.1%
    }

    #[test]
    fn conf_bps_zero_price_saturates() {
        let p = PythPrice {
            feed_id: [0; 32], price: 0, conf: 1000, expo: -8,
            publish_time: 0, prev_publish_time: 0, ema_price: 0, ema_conf: 0, posted_slot: 0,
        };
        assert_eq!(p.conf_bps(), u32::MAX);
    }

    #[test]
    fn age_seconds_handles_clock_skew() {
        let p = PythPrice {
            feed_id: [0; 32], price: 1, conf: 0, expo: 0,
            publish_time: 1000, prev_publish_time: 999,
            ema_price: 1, ema_conf: 0, posted_slot: 0,
        };
        assert_eq!(p.age_seconds(1042), 42);
        assert_eq!(p.age_seconds(900), 0, "future publish_time clamps to zero");
    }

    #[test]
    fn feed_for_symbol_mainnet_known_assets() {
        for sym in ["SOL", "USDC", "USDT", "BTC", "ETH", "JITOSOL", "INF", "BSOL"] {
            assert!(feed_for_symbol(sym, false).is_some(), "missing mainnet {sym}");
        }
        assert!(feed_for_symbol("XYZ", false).is_none());
    }

    #[test]
    fn feed_for_symbol_devnet_returns_none() {
        // Devnet uses a different keeper set; we don't have those addresses
        // mapped yet. Return None rather than silently giving the wrong account.
        assert!(feed_for_symbol("SOL", true).is_none());
        assert!(feed_for_symbol("USDC", true).is_none());
    }

    #[test]
    fn feed_lookup_is_case_insensitive() {
        assert_eq!(feed_for_symbol("sol", false), feed_for_symbol("SOL", false));
        assert_eq!(feed_for_symbol("UsDc", false), feed_for_symbol("USDC", false));
    }

    #[test]
    fn sol_address_matches_verified_sponsored_account() {
        // Hardcoded mainnet address — verified live on 2026-05-03 by
        // querying getProgramAccounts for the SOL/USD feed_id and picking
        // the highest-posted_slot result.
        assert_eq!(
            feed_for_symbol("SOL", false).unwrap().to_string(),
            "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE",
        );
    }

    #[test]
    fn feed_ids_round_trip_for_known_assets() {
        for sym in ["SOL", "USDC", "BTC", "ETH", "JITOSOL", "INF"] {
            let id = feed_id_for_symbol(sym).unwrap_or_else(|| panic!("missing {sym}"));
            assert_eq!(id.len(), 32);
        }
    }
}
