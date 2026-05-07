//! Pure-logic unit tests for `riskwatcher_daemon::thresholds`.
//!
//! M5 scope: no I/O, no async. Each test constructs a synthetic
//! `DecodedObligation` directly and asserts the classifier's verdict.

use solana_sdk::pubkey::Pubkey;

use riskwatcher_daemon::state::{PositionView, Source};
use riskwatcher_daemon::thresholds::{
    classify, DISTANCE_CRITICAL_BPS, DISTANCE_NOTICE_BPS, DISTANCE_WARNING_BPS,
};
use zerox1_defi_protocols::protocols::kamino_loader::DecodedObligation;
use zerox1_protocol::fleet::riskwatcher::RiskSeverity;

fn make_view() -> PositionView {
    PositionView {
        subject: [0u8; 32],
        obligation_pubkey: Pubkey::default(),
        last_ltv_bps: 0,
        last_seen_unix: 0,
        source: Source::Poll,
    }
}

/// Build a `DecodedObligation` with the desired `(borrowed, unhealthy)`
/// values; everything else is zeroed/empty. The other fields don't
/// influence `classify`.
fn make_obligation(borrowed_sf: u128, unhealthy_sf: u128) -> DecodedObligation {
    DecodedObligation {
        address: Pubkey::default(),
        lending_market: Pubkey::default(),
        owner: Pubkey::default(),
        deposits: Vec::new(),
        borrows: Vec::new(),
        deposited_value_sf: 0,
        borrow_factor_adjusted_debt_value_sf: 0,
        borrowed_assets_market_value_sf: borrowed_sf,
        allowed_borrow_value_sf: 0,
        unhealthy_borrow_value_sf: unhealthy_sf,
    }
}

/// Build an obligation with a target distance, expressed in bps.
/// Picks `unhealthy = 1_000_000`, `borrowed = unhealthy * (10_000 - bps)
/// / 10_000` so the resulting integer ratio matches `bps` exactly.
fn obligation_with_distance_bps(target_bps: u128) -> DecodedObligation {
    let unhealthy = 1_000_000u128;
    let borrowed = unhealthy * (10_000 - target_bps) / 10_000;
    make_obligation(borrowed, unhealthy)
}

#[test]
fn comfortable_position_returns_none() {
    // 25% headroom — well above NOTICE (5%).
    let view = make_view();
    let ob = obligation_with_distance_bps(2_500);
    assert_eq!(classify(&view, &ob), None);
}

#[test]
fn notice_band_returns_notice() {
    // 350 bps headroom — strictly between WARNING (200) and NOTICE (500).
    let view = make_view();
    let ob = obligation_with_distance_bps(350);
    assert_eq!(classify(&view, &ob), Some(RiskSeverity::Notice));
}

#[test]
fn warning_band_returns_warning() {
    // 100 bps headroom — strictly between CRITICAL (50) and WARNING (200).
    let view = make_view();
    let ob = obligation_with_distance_bps(100);
    assert_eq!(classify(&view, &ob), Some(RiskSeverity::Warning));
}

#[test]
fn critical_band_returns_critical() {
    // 25 bps headroom — strictly below CRITICAL (50).
    let view = make_view();
    let ob = obligation_with_distance_bps(25);
    assert_eq!(classify(&view, &ob), Some(RiskSeverity::Critical));
}

#[test]
fn at_or_past_liquidation_returns_critical() {
    // borrowed >= unhealthy → distance 0 → falls into Critical.
    let view = make_view();
    let ob = make_obligation(1_500_000, 1_000_000);
    assert_eq!(classify(&view, &ob), Some(RiskSeverity::Critical));
}

#[test]
fn zero_exposure_returns_none() {
    // No collateral / no liquidation threshold → nothing to classify.
    let view = make_view();
    let ob = make_obligation(0, 0);
    assert_eq!(classify(&view, &ob), None);
}

#[test]
fn boundary_at_warning_classifies_as_notice() {
    // Distance == DISTANCE_WARNING_BPS exactly: bands are exclusive at
    // the upper edge, so this lands in Notice, not Warning.
    let view = make_view();
    let ob = obligation_with_distance_bps(u128::from(DISTANCE_WARNING_BPS));
    assert_eq!(classify(&view, &ob), Some(RiskSeverity::Notice));

    // Sibling sanity-checks for the other two boundaries:
    // - At NOTICE exactly → None (no band).
    let ob_notice = obligation_with_distance_bps(u128::from(DISTANCE_NOTICE_BPS));
    assert_eq!(classify(&view, &ob_notice), None);
    // - At CRITICAL exactly → Warning (one band up from Critical).
    let ob_crit = obligation_with_distance_bps(u128::from(DISTANCE_CRITICAL_BPS));
    assert_eq!(classify(&view, &ob_crit), Some(RiskSeverity::Warning));
}
