//! Pure portfolio-delta math.
//!
//! Given JLP holdings + per-custody USD exposures + total JLP supply,
//! compute how much SOL/ETH/BTC long exposure (in USD) the holder has.
//! The rebalancer (M9) calls this to decide hedge sizing.
//!
//! All math is in u128 intermediates to avoid overflow on large pools
//! ($1B+ AUM × small share fits comfortably; the multiplication
//! `usd_value × our_jlp` does not).
//!
//! Mint bucketing uses well-known mainnet pubkeys re-exported from
//! `zerox1_defi_protocols::constants`:
//!   - `WSOL_MINT`        → SOL bucket (non-stable)
//!   - `WETH_PORTAL_MINT` → ETH bucket (non-stable)
//!   - `WBTC_PORTAL_MINT` → BTC bucket (non-stable)
//!   - `USDC_MINT`        → stable bucket
//!   - `USDT_MINT`        → stable bucket
//!   - any other          → falls into the stable bucket (a custody whose
//!                          `is_stable=true` flag is set in the on-chain
//!                          custody header always lands in the stable
//!                          bucket regardless of mint).
//!
//! ## fleet-v0.4.0-rc7: pubkey-aware bucketing
//!
//! `compute_delta_with_pubkeys` is a stricter variant that ALSO accepts
//! the custody pubkey for each exposure (parallel vec, same length /
//! order). When a non-stable mint doesn't match WSOL/WETH/WBTC_PORTAL,
//! the function falls back to matching the custody pubkey against the
//! known JLP SOL/ETH/BTC custody PDAs (stable since Jupiter Perps
//! launch). This guards against:
//!   - a future Jupiter Perps custody migration where the underlying
//!     mint changes but the custody pubkey stays stable;
//!   - a decoder regression where the on-chain mint reads back as
//!     `Pubkey::default()` or shifts by an offset;
//!   - any other off-by-one in the mint-to-asset mapping.
//!
//! `compute_delta` (the original mint-only path) is retained for tests
//! and the pre-rc7 buy-leg synthetic path that doesn't have custody
//! pubkeys in scope.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use tracing::warn;

use zerox1_defi_protocols::constants::{WBTC_PORTAL_MINT, WETH_PORTAL_MINT, WSOL_MINT};

/// JLP pool's SOL custody PDA. Stable since Jupiter Perps program
/// launch — verified 2026-05-18 against the live `jlp-info` endpoint
/// (`https://perps-api.jup.ag/v1/jlp-info`) which returned this exact
/// pubkey as the SOL-marketed custody. The same value lives in
/// `crate::jlp_hedge::JLP_SOL_CUSTODY` and the dashboard's
/// `chain/jupiter_perps.rs::SOL_CUSTODY_STR`; the duplication here
/// keeps `delta.rs` self-contained (no upstream module dependency
/// loop).
const JLP_SOL_CUSTODY: Pubkey =
    solana_sdk::pubkey!("7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz");
/// JLP pool's ETH custody PDA. Same source as `JLP_SOL_CUSTODY`.
const JLP_ETH_CUSTODY: Pubkey =
    solana_sdk::pubkey!("AQCGyheWPLeo6Qp9WpYS9m3Qj479t7R636N9ey1rEjEn");
/// JLP pool's BTC custody PDA. Same source as `JLP_SOL_CUSTODY`.
const JLP_BTC_CUSTODY: Pubkey =
    solana_sdk::pubkey!("5Pv3gM9JrFFH883SWAhvJC9RPYmo8UNxuFtv5bMMALkm");

/// One custody's contribution to JLP NAV. The caller computes
/// `usd_value` as `(owned - locked_unbacked) * price - guaranteed_usd_of_shorts`
/// — but the math here is bucketing-only and treats `usd_value` as
/// authoritative in micro-USD ($1 = 1_000_000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustodyExposure {
    pub mint: Pubkey,
    pub usd_value: u64,
    pub is_stable: bool,
}

/// Computed portfolio delta given pro-rata JLP ownership.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortfolioDelta {
    /// Per-asset USD exposure (micro-USD) attributable to our JLP holdings.
    pub sol_usd: u64,
    pub eth_usd: u64,
    pub btc_usd: u64,
    pub stable_usd: u64,
    /// Total USD value of our JLP holdings = sum of the four buckets.
    pub total_usd: u64,
    /// Net non-stable exposure (sol + eth + btc) as bps of total.
    /// 0 = all stable; 10_000 = entirely non-stable.
    pub long_exposure_bps: u16,
}

/// Compute pro-rata delta given total pool exposures + our share.
///
/// `our_jlp_lamports` and `total_jlp_supply` are raw JLP token units
/// (JLP has 6 decimals on mainnet).
///
/// Errors if `total_jlp_supply == 0`. Returns an all-zero delta if our
/// holdings are zero (no exposure → 0 bps long exposure).
pub fn compute_delta(
    custodies: &[CustodyExposure],
    our_jlp_lamports: u64,
    total_jlp_supply: u64,
) -> Result<PortfolioDelta> {
    if total_jlp_supply == 0 {
        bail!("total_jlp_supply is zero");
    }
    if our_jlp_lamports == 0 {
        return Ok(PortfolioDelta {
            sol_usd: 0,
            eth_usd: 0,
            btc_usd: 0,
            stable_usd: 0,
            total_usd: 0,
            long_exposure_bps: 0,
        });
    }

    let mut sol_usd: u128 = 0;
    let mut eth_usd: u128 = 0;
    let mut btc_usd: u128 = 0;
    let mut stable_usd: u128 = 0;

    let our_share = our_jlp_lamports as u128;
    let total = total_jlp_supply as u128;

    for c in custodies {
        // Pro-rata: our_share / total. Done as
        //   (usd_value * our_share) / total
        // in u128 to avoid overflow on large pools (worst-case
        // 2^64 * 2^64 / 2^64 = 2^64 which fits u128 trivially).
        let pro_rata = (c.usd_value as u128).saturating_mul(our_share) / total;

        if c.is_stable {
            stable_usd = stable_usd.saturating_add(pro_rata);
        } else if c.mint == WSOL_MINT {
            sol_usd = sol_usd.saturating_add(pro_rata);
        } else if c.mint == WETH_PORTAL_MINT {
            eth_usd = eth_usd.saturating_add(pro_rata);
        } else if c.mint == WBTC_PORTAL_MINT {
            btc_usd = btc_usd.saturating_add(pro_rata);
        } else {
            // Unknown non-stable mint: bucket as stable to be
            // conservative (the rebalancer won't try to hedge it).
            // A real new asset would need an explicit code path; the
            // alternative (silently bucketing as SOL/ETH/BTC) would
            // be unsafe.
            stable_usd = stable_usd.saturating_add(pro_rata);
        }
    }

    let non_stable = sol_usd.saturating_add(eth_usd).saturating_add(btc_usd);
    let total_u128 = non_stable.saturating_add(stable_usd);

    let long_exposure_bps = if total_u128 > 0 {
        ((non_stable.saturating_mul(10_000)) / total_u128).min(10_000) as u16
    } else {
        0
    };

    Ok(PortfolioDelta {
        sol_usd: u64_clip(sol_usd),
        eth_usd: u64_clip(eth_usd),
        btc_usd: u64_clip(btc_usd),
        stable_usd: u64_clip(stable_usd),
        total_usd: u64_clip(total_u128),
        long_exposure_bps,
    })
}

#[inline]
fn u64_clip(v: u128) -> u64 {
    v.min(u64::MAX as u128) as u64
}

/// Defensive bucketing helper. Given a custody's pubkey, decoded mint,
/// and `is_stable` flag, decide which `PortfolioDelta` bucket the
/// custody belongs to: `SOL`, `ETH`, `BTC`, or `Stable`.
///
/// Match order:
///   1. `is_stable=true` → `Stable`
///   2. mint matches `WSOL_MINT` → `SOL`
///   3. mint matches `WETH_PORTAL_MINT` → `ETH`
///   4. mint matches `WBTC_PORTAL_MINT` → `BTC`
///   5. custody pubkey matches `JLP_SOL_CUSTODY` → `SOL` (fallback)
///   6. custody pubkey matches `JLP_ETH_CUSTODY` → `ETH` (fallback)
///   7. custody pubkey matches `JLP_BTC_CUSTODY` → `BTC` (fallback)
///   8. otherwise → `Stable` (conservative; rebalancer won't hedge it)
///
/// Steps 5-7 are the fleet-v0.4.0-rc7 defense. If a future Jupiter
/// Perps custody migration changes the underlying mint, OR if the
/// custody decoder hits an off-by-one and the decoded mint differs
/// from the constant, the well-known custody pubkey still anchors the
/// asset to the right bucket. Each fallback hit logs a WARN so
/// operators get a signal that the mint constants need refreshing.
fn classify_custody(
    custody_pubkey: &Pubkey,
    mint: &Pubkey,
    is_stable: bool,
) -> Bucket {
    if is_stable {
        return Bucket::Stable;
    }
    if *mint == WSOL_MINT {
        return Bucket::Sol;
    }
    if *mint == WETH_PORTAL_MINT {
        return Bucket::Eth;
    }
    if *mint == WBTC_PORTAL_MINT {
        return Bucket::Btc;
    }
    // Mint didn't match any known constant. Fall back to custody pubkey.
    if *custody_pubkey == JLP_SOL_CUSTODY {
        warn!(
            %custody_pubkey,
            %mint,
            "delta bucketing fallback: custody pubkey matches JLP_SOL_CUSTODY but decoded mint \
             is not WSOL_MINT — refresh constants if Jupiter rotated custody assets"
        );
        return Bucket::Sol;
    }
    if *custody_pubkey == JLP_ETH_CUSTODY {
        warn!(
            %custody_pubkey,
            %mint,
            "delta bucketing fallback: custody pubkey matches JLP_ETH_CUSTODY but decoded mint \
             is not WETH_PORTAL_MINT — refresh constants if Jupiter rotated custody assets"
        );
        return Bucket::Eth;
    }
    if *custody_pubkey == JLP_BTC_CUSTODY {
        warn!(
            %custody_pubkey,
            %mint,
            "delta bucketing fallback: custody pubkey matches JLP_BTC_CUSTODY but decoded mint \
             is not WBTC_PORTAL_MINT — refresh constants if Jupiter rotated custody assets"
        );
        return Bucket::Btc;
    }
    // Unknown non-stable custody. Conservative: stable bucket (won't be
    // hedged). Logged WARN so operators see a real new asset surface.
    warn!(
        %custody_pubkey,
        %mint,
        "delta bucketing: non-stable custody with unknown mint AND unknown custody pubkey — \
         contributing to stable bucket (won't be hedged)"
    );
    Bucket::Stable
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bucket {
    Sol,
    Eth,
    Btc,
    Stable,
}

/// Pubkey-aware variant of `compute_delta`. Accepts `custody_pubkeys`
/// parallel to `custodies` (same length, same order). Each exposure is
/// bucketed via `classify_custody` which falls back to matching the
/// custody pubkey against the well-known JLP custody PDAs if the
/// decoded mint doesn't resolve to a known constant — fleet-v0.4.0-rc7
/// defense against Jupiter Perps custody migrations + decoder
/// regressions.
///
/// Errors if `total_jlp_supply == 0` OR `custody_pubkeys.len() !=
/// custodies.len()`. Returns an all-zero delta if our holdings are
/// zero (mirrors `compute_delta`).
pub fn compute_delta_with_pubkeys(
    custodies: &[CustodyExposure],
    custody_pubkeys: &[Pubkey],
    our_jlp_lamports: u64,
    total_jlp_supply: u64,
) -> Result<PortfolioDelta> {
    if total_jlp_supply == 0 {
        bail!("total_jlp_supply is zero");
    }
    if custody_pubkeys.len() != custodies.len() {
        bail!(
            "custody_pubkeys length {} != custodies length {} — parallel vecs must match",
            custody_pubkeys.len(),
            custodies.len()
        );
    }
    if our_jlp_lamports == 0 {
        return Ok(PortfolioDelta {
            sol_usd: 0,
            eth_usd: 0,
            btc_usd: 0,
            stable_usd: 0,
            total_usd: 0,
            long_exposure_bps: 0,
        });
    }

    let mut sol_usd: u128 = 0;
    let mut eth_usd: u128 = 0;
    let mut btc_usd: u128 = 0;
    let mut stable_usd: u128 = 0;

    let our_share = our_jlp_lamports as u128;
    let total = total_jlp_supply as u128;

    for (cp, c) in custody_pubkeys.iter().zip(custodies.iter()) {
        let pro_rata = (c.usd_value as u128).saturating_mul(our_share) / total;
        match classify_custody(cp, &c.mint, c.is_stable) {
            Bucket::Sol => sol_usd = sol_usd.saturating_add(pro_rata),
            Bucket::Eth => eth_usd = eth_usd.saturating_add(pro_rata),
            Bucket::Btc => btc_usd = btc_usd.saturating_add(pro_rata),
            Bucket::Stable => stable_usd = stable_usd.saturating_add(pro_rata),
        }
    }

    let non_stable = sol_usd.saturating_add(eth_usd).saturating_add(btc_usd);
    let total_u128 = non_stable.saturating_add(stable_usd);

    let long_exposure_bps = if total_u128 > 0 {
        ((non_stable.saturating_mul(10_000)) / total_u128).min(10_000) as u16
    } else {
        0
    };

    Ok(PortfolioDelta {
        sol_usd: u64_clip(sol_usd),
        eth_usd: u64_clip(eth_usd),
        btc_usd: u64_clip(btc_usd),
        stable_usd: u64_clip(stable_usd),
        total_usd: u64_clip(total_u128),
        long_exposure_bps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_on_zero_supply() {
        let err = compute_delta(&[], 1, 0).unwrap_err();
        assert!(err.to_string().contains("zero"));
    }

    #[test]
    fn zero_holdings_yields_zero_exposure() {
        let custodies = vec![CustodyExposure {
            mint: WSOL_MINT,
            usd_value: 1_000_000_000,
            is_stable: false,
        }];
        let d = compute_delta(&custodies, 0, 1_000_000_000).unwrap();
        assert_eq!(d.total_usd, 0);
        assert_eq!(d.long_exposure_bps, 0);
    }

    #[test]
    fn unknown_non_stable_mint_buckets_as_stable() {
        let custodies = vec![CustodyExposure {
            mint: Pubkey::new_unique(),
            usd_value: 1_000_000_000,
            is_stable: false,
        }];
        let d = compute_delta(&custodies, 100_000_000, 1_000_000_000).unwrap();
        // Unknown mint with is_stable=false falls into stable bucket
        // (conservative — won't be hedged).
        assert_eq!(d.sol_usd, 0);
        assert_eq!(d.stable_usd, 100_000_000);
        assert_eq!(d.long_exposure_bps, 0);
    }

    // ── compute_delta_with_pubkeys — fleet-v0.4.0-rc7 defense ──────────

    #[test]
    fn compute_delta_with_pubkeys_buckets_by_mint_when_mints_match() {
        // Happy path: mints match the constants, bucketing works as
        // expected (same as the original compute_delta).
        let pubkeys = vec![
            JLP_SOL_CUSTODY,
            JLP_ETH_CUSTODY,
            JLP_BTC_CUSTODY,
            Pubkey::new_unique(), // USDC custody (any pubkey for stable)
        ];
        let custodies = vec![
            CustodyExposure {
                mint: WSOL_MINT,
                usd_value: 47_000_000,
                is_stable: false,
            },
            CustodyExposure {
                mint: WETH_PORTAL_MINT,
                usd_value: 10_000_000,
                is_stable: false,
            },
            CustodyExposure {
                mint: WBTC_PORTAL_MINT,
                usd_value: 10_000_000,
                is_stable: false,
            },
            CustodyExposure {
                mint: Pubkey::new_unique(),
                usd_value: 33_000_000,
                is_stable: true,
            },
        ];
        let d = compute_delta_with_pubkeys(&custodies, &pubkeys, 1, 1).unwrap();
        assert_eq!(d.sol_usd, 47_000_000);
        assert_eq!(d.eth_usd, 10_000_000);
        assert_eq!(d.btc_usd, 10_000_000);
        assert_eq!(d.stable_usd, 33_000_000);
    }

    #[test]
    fn compute_delta_with_pubkeys_falls_back_to_custody_pubkey_when_mint_unknown() {
        // The fleet-v0.4.0-rc7 defense: a non-stable custody whose
        // decoded mint isn't WSOL/WETH/WBTC_PORTAL but whose custody
        // pubkey IS the well-known JLP_SOL_CUSTODY → bucket as SOL.
        let pubkeys = vec![JLP_SOL_CUSTODY];
        let custodies = vec![CustodyExposure {
            // A made-up mint — pretend Jupiter rotated to a new SOL
            // wrapper. The custody pubkey is unchanged.
            mint: Pubkey::new_unique(),
            usd_value: 50_000_000,
            is_stable: false,
        }];
        let d = compute_delta_with_pubkeys(&custodies, &pubkeys, 1, 1).unwrap();
        assert_eq!(
            d.sol_usd, 50_000_000,
            "custody-pubkey fallback must bucket as SOL even when mint is unrecognised"
        );
        assert_eq!(d.stable_usd, 0);
        assert_eq!(d.long_exposure_bps, 10_000);
    }

    #[test]
    fn compute_delta_with_pubkeys_eth_btc_fallbacks() {
        // Pin: ETH and BTC fallbacks fire for the right buckets.
        let pubkeys = vec![JLP_ETH_CUSTODY, JLP_BTC_CUSTODY];
        let custodies = vec![
            CustodyExposure {
                mint: Pubkey::new_unique(), // unknown
                usd_value: 20_000_000,
                is_stable: false,
            },
            CustodyExposure {
                mint: Pubkey::new_unique(), // unknown
                usd_value: 30_000_000,
                is_stable: false,
            },
        ];
        let d = compute_delta_with_pubkeys(&custodies, &pubkeys, 1, 1).unwrap();
        assert_eq!(d.eth_usd, 20_000_000);
        assert_eq!(d.btc_usd, 30_000_000);
        assert_eq!(d.sol_usd, 0);
    }

    #[test]
    fn compute_delta_with_pubkeys_unknown_pubkey_and_mint_falls_to_stable() {
        // Truly unknown (neither pubkey nor mint matches anything
        // known): conservative — stable bucket. Logged WARN.
        let pubkeys = vec![Pubkey::new_unique()];
        let custodies = vec![CustodyExposure {
            mint: Pubkey::new_unique(),
            usd_value: 100_000_000,
            is_stable: false,
        }];
        let d = compute_delta_with_pubkeys(&custodies, &pubkeys, 1, 1).unwrap();
        assert_eq!(d.sol_usd, 0);
        assert_eq!(d.eth_usd, 0);
        assert_eq!(d.btc_usd, 0);
        assert_eq!(d.stable_usd, 100_000_000);
    }

    #[test]
    fn compute_delta_with_pubkeys_is_stable_short_circuits_pubkey_fallback() {
        // Pin: an is_stable=true custody NEVER triggers the pubkey
        // fallback even if its pubkey accidentally equals one of the
        // JLP non-stable custody PDAs. Defense against a future bug
        // where a USDC custody pubkey collides.
        let pubkeys = vec![JLP_SOL_CUSTODY]; // deliberately wrong
        let custodies = vec![CustodyExposure {
            mint: Pubkey::new_unique(),
            usd_value: 100_000_000,
            is_stable: true, // overrides everything
        }];
        let d = compute_delta_with_pubkeys(&custodies, &pubkeys, 1, 1).unwrap();
        assert_eq!(d.sol_usd, 0);
        assert_eq!(d.stable_usd, 100_000_000);
    }

    #[test]
    fn compute_delta_with_pubkeys_rejects_length_mismatch() {
        // Defensive: bail loudly when caller passes mismatched
        // parallel vecs (would otherwise produce a silent zip-shortest
        // truncation bug).
        let pubkeys = vec![JLP_SOL_CUSTODY, JLP_ETH_CUSTODY];
        let custodies = vec![CustodyExposure {
            mint: WSOL_MINT,
            usd_value: 100,
            is_stable: false,
        }];
        let err = compute_delta_with_pubkeys(&custodies, &pubkeys, 1, 1).unwrap_err();
        assert!(err.to_string().contains("parallel vecs"));
    }

    #[test]
    fn compute_delta_with_pubkeys_prod_174_shape_recovers_stable_bucket() {
        // Reproduce the rc7 prod incident's input shape: $174 JLP,
        // pool composition ~47% SOL, ~7% ETH, ~16% BTC, balance stable
        // (matches actual JLP weights from 2026-05-18 jlp-info).
        // With the fixed bucketing, stable_usd MUST be non-zero —
        // exactly the failure mode rc7 hit (long_bps=10000 in prod).
        let pubkeys = vec![
            JLP_SOL_CUSTODY,
            JLP_ETH_CUSTODY,
            JLP_BTC_CUSTODY,
            Pubkey::new_unique(), // USDC custody
            Pubkey::new_unique(), // USDT custody
        ];
        let custodies = vec![
            CustodyExposure {
                mint: WSOL_MINT,
                usd_value: 82_000_000, // $82
                is_stable: false,
            },
            CustodyExposure {
                mint: WETH_PORTAL_MINT,
                usd_value: 12_000_000, // $12
                is_stable: false,
            },
            CustodyExposure {
                mint: WBTC_PORTAL_MINT,
                usd_value: 28_000_000, // $28
                is_stable: false,
            },
            CustodyExposure {
                mint: Pubkey::new_unique(),
                usd_value: 50_000_000, // $50 USDC
                is_stable: true,
            },
            CustodyExposure {
                mint: Pubkey::new_unique(),
                usd_value: 2_000_000, // $2 USDT
                is_stable: true,
            },
        ];
        let d = compute_delta_with_pubkeys(&custodies, &pubkeys, 1, 1).unwrap();
        assert_eq!(d.sol_usd, 82_000_000);
        assert_eq!(d.eth_usd, 12_000_000);
        assert_eq!(d.btc_usd, 28_000_000);
        assert_eq!(d.stable_usd, 52_000_000);
        assert_eq!(d.total_usd, 174_000_000);
        // long_exposure_bps = 122 / 174 ≈ 7011 — explicitly NOT 10000.
        assert!(
            d.long_exposure_bps > 6_900 && d.long_exposure_bps < 7_100,
            "long_exposure_bps should be ~7000 with stable bucket populated, got {}",
            d.long_exposure_bps
        );
    }
}
