//! Comprehensive `compute_delta` tests over synthetic JLP compositions.
//!
//! Mirrors a realistic mainnet JLP composition (~45% SOL, 10% ETH, 10% BTC,
//! 25% USDC, 9% USDT, balance other) plus edge cases: pure-stable, pure-SOL,
//! zero supply, zero holdings, pro-rata scaling, summation invariants.

use hedgedjlp_daemon::delta::{compute_delta, CustodyExposure};
use solana_sdk::pubkey::Pubkey;
use zerox1_defi_protocols::constants::{
    USDC_MINT, USDT_MINT, WBTC_PORTAL_MINT, WETH_PORTAL_MINT, WSOL_MINT,
};

fn sol_custody(usd: u64) -> CustodyExposure {
    CustodyExposure {
        mint: WSOL_MINT,
        usd_value: usd,
        is_stable: false,
    }
}
fn eth_custody(usd: u64) -> CustodyExposure {
    CustodyExposure {
        mint: WETH_PORTAL_MINT,
        usd_value: usd,
        is_stable: false,
    }
}
fn btc_custody(usd: u64) -> CustodyExposure {
    CustodyExposure {
        mint: WBTC_PORTAL_MINT,
        usd_value: usd,
        is_stable: false,
    }
}
fn usdc_custody(usd: u64) -> CustodyExposure {
    CustodyExposure {
        mint: USDC_MINT,
        usd_value: usd,
        is_stable: true,
    }
}
fn usdt_custody(usd: u64) -> CustodyExposure {
    CustodyExposure {
        mint: USDT_MINT,
        usd_value: usd,
        is_stable: true,
    }
}

#[test]
fn pure_stable_pool_yields_zero_long_exposure() {
    let custodies = vec![usdc_custody(1_000_000_000)];
    let d = compute_delta(&custodies, 100_000_000, 1_000_000_000).unwrap();
    assert_eq!(d.long_exposure_bps, 0);
    assert_eq!(d.sol_usd, 0);
    assert_eq!(d.eth_usd, 0);
    assert_eq!(d.btc_usd, 0);
    assert_eq!(d.stable_usd, 100_000_000); // 10% of $1B = $100M
    assert_eq!(d.total_usd, 100_000_000);
}

#[test]
fn pure_sol_pool_yields_full_long_exposure() {
    let custodies = vec![sol_custody(1_000_000_000)];
    let d = compute_delta(&custodies, 100_000_000, 1_000_000_000).unwrap();
    assert_eq!(d.long_exposure_bps, 10_000);
    assert_eq!(d.sol_usd, 100_000_000);
    assert_eq!(d.stable_usd, 0);
}

#[test]
fn realistic_jlp_mix_lands_in_expected_band() {
    // Realistic-ish mainnet composition: 45% SOL, 10% ETH, 10% BTC,
    // 25% USDC, 10% USDT. Non-stable share = 65%.
    let custodies = vec![
        sol_custody(450_000_000),
        eth_custody(100_000_000),
        btc_custody(100_000_000),
        usdc_custody(250_000_000),
        usdt_custody(100_000_000),
    ];
    let d = compute_delta(&custodies, 100_000_000, 1_000_000_000).unwrap();
    // 65% non-stable → ~6500 bps
    assert!(
        d.long_exposure_bps > 6_400 && d.long_exposure_bps < 6_600,
        "expected ~6500 bps, got {}",
        d.long_exposure_bps
    );
    // Per-bucket sanity: 10% pro-rata of each.
    assert_eq!(d.sol_usd, 45_000_000);
    assert_eq!(d.eth_usd, 10_000_000);
    assert_eq!(d.btc_usd, 10_000_000);
    assert_eq!(d.stable_usd, 35_000_000);
    assert_eq!(d.total_usd, 100_000_000);
}

#[test]
fn pro_rata_scales_linearly() {
    let custodies = vec![sol_custody(1_000_000_000)];
    // 50% ownership = half the exposure.
    let d = compute_delta(&custodies, 500_000_000, 1_000_000_000).unwrap();
    assert_eq!(d.sol_usd, 500_000_000);
    assert_eq!(d.long_exposure_bps, 10_000);
}

#[test]
fn zero_jlp_supply_errors() {
    let custodies = vec![sol_custody(1_000_000_000)];
    let err = compute_delta(&custodies, 100_000_000, 0).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("zero"),
        "expected zero-supply error, got: {err}"
    );
}

#[test]
fn zero_holdings_yields_zero_exposure() {
    let custodies = vec![sol_custody(1_000_000_000), usdc_custody(1_000_000_000)];
    let d = compute_delta(&custodies, 0, 1_000_000_000).unwrap();
    assert_eq!(d.total_usd, 0);
    assert_eq!(d.long_exposure_bps, 0);
    assert_eq!(d.sol_usd, 0);
    assert_eq!(d.stable_usd, 0);
}

#[test]
fn buckets_sum_to_total() {
    let custodies = vec![
        sol_custody(450_000_000),
        eth_custody(100_000_000),
        btc_custody(100_000_000),
        usdc_custody(250_000_000),
        usdt_custody(100_000_000),
    ];
    let d = compute_delta(&custodies, 100_000_000, 1_000_000_000).unwrap();
    assert_eq!(
        d.sol_usd + d.eth_usd + d.btc_usd + d.stable_usd,
        d.total_usd,
        "buckets must sum to total"
    );
}

#[test]
fn long_exposure_bps_clamps_to_10000() {
    // Pure non-stable, no stable counterweight → exactly 10_000 bps.
    let custodies = vec![
        sol_custody(500_000_000),
        eth_custody(300_000_000),
        btc_custody(200_000_000),
    ];
    let d = compute_delta(&custodies, 1_000_000_000, 1_000_000_000).unwrap();
    assert_eq!(d.long_exposure_bps, 10_000);
}

#[test]
fn full_ownership_recovers_full_pool_value() {
    // Holding 100% of supply should recover the full per-custody usd_value.
    let custodies = vec![
        sol_custody(450_000_000),
        eth_custody(100_000_000),
        btc_custody(100_000_000),
        usdc_custody(350_000_000),
    ];
    let total_supply = 1_000_000_000_u64;
    let d = compute_delta(&custodies, total_supply, total_supply).unwrap();
    assert_eq!(d.sol_usd, 450_000_000);
    assert_eq!(d.eth_usd, 100_000_000);
    assert_eq!(d.btc_usd, 100_000_000);
    assert_eq!(d.stable_usd, 350_000_000);
    assert_eq!(d.total_usd, 1_000_000_000);
}

#[test]
fn unknown_mint_falls_to_stable_bucket_conservatively() {
    // A never-seen mint with is_stable=false must NOT silently land in
    // SOL/ETH/BTC. It falls to stable so the rebalancer ignores it.
    let unknown = CustodyExposure {
        mint: Pubkey::new_unique(),
        usd_value: 1_000_000_000,
        is_stable: false,
    };
    let d = compute_delta(&[unknown], 100_000_000, 1_000_000_000).unwrap();
    assert_eq!(d.sol_usd, 0);
    assert_eq!(d.eth_usd, 0);
    assert_eq!(d.btc_usd, 0);
    assert_eq!(d.stable_usd, 100_000_000);
    assert_eq!(d.long_exposure_bps, 0);
}

#[test]
fn empty_custody_list_yields_zero_total() {
    let d = compute_delta(&[], 100_000_000, 1_000_000_000).unwrap();
    assert_eq!(d.total_usd, 0);
    assert_eq!(d.long_exposure_bps, 0);
}

#[test]
fn large_pool_no_overflow() {
    // Stress: $1B pool with 1B JLP supply, 100M held.
    // u128 intermediate should comfortably handle the multiplication.
    let custodies = vec![sol_custody(u64::MAX / 4), usdc_custody(u64::MAX / 4)];
    // Even with massive usd_values we should not panic.
    let d = compute_delta(&custodies, 1, 1_000_000_000).unwrap();
    // sanity — no panic, valid bps
    assert!(d.long_exposure_bps <= 10_000);
}
