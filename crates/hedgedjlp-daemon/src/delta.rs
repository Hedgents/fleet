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

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use zerox1_defi_protocols::constants::{WBTC_PORTAL_MINT, WETH_PORTAL_MINT, WSOL_MINT};

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

    let non_stable = sol_usd
        .saturating_add(eth_usd)
        .saturating_add(btc_usd);
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
}
