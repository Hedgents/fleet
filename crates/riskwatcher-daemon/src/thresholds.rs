//! Risk classification by liquidation-distance bands.
//!
//! Pure-logic module: no I/O, no async, no network. Given a
//! [`PositionView`] (which the registry already has) and a freshly-decoded
//! [`DecodedObligation`] (which the M4 poller already fetches), return
//! `Some(RiskSeverity)` when the position has crossed a band threshold,
//! else `None`.
//!
//! Distance is computed in basis points as
//!
//! ```text
//! distance_bps = (unhealthy_borrow_value_sf - borrowed_assets_market_value_sf) * 10_000
//!                / unhealthy_borrow_value_sf
//! ```
//!
//! Both values are sf-scaled (multiply by 2^60 for true value), but the
//! scaling cancels in the ratio — distance is pure integer arithmetic.
//!
//! Bands are exclusive at the upper edge: a position at exactly
//! `DISTANCE_WARNING_BPS` falls in the Notice band, not Warning. This is
//! deliberate so that the constants describe the *floor* of headroom for
//! their adjacent (less severe) band.

use zerox1_defi_protocols::protocols::kamino_loader::DecodedObligation;
use zerox1_protocol::fleet::riskwatcher::RiskSeverity;

use crate::state::PositionView;

/// Notice band — informational headroom (≤ 5%, > Warning).
pub const DISTANCE_NOTICE_BPS: u16 = 500;

/// Warning band — escalate to orchestrator (≤ 2%, > Critical).
pub const DISTANCE_WARNING_BPS: u16 = 200;

/// Critical band — soft-veto further leverage (≤ 0.5%).
pub const DISTANCE_CRITICAL_BPS: u16 = 50;

/// Compute liquidation-distance in basis points.
///
/// Returns `None` if the obligation has no exposure
/// (`unhealthy_borrow_value_sf == 0`) — there is nothing to liquidate.
///
/// If `borrowed_assets_market_value_sf >= unhealthy_borrow_value_sf` the
/// position is at or past the liquidation threshold; saturating subtraction
/// yields a distance of zero, which the classifier treats as Critical.
///
/// The intermediate `(diff * 10_000)` uses saturating multiplication —
/// realistic obligation values stay well below 2^96, so saturation is
/// defensive rather than load-bearing.
pub fn distance_bps(decoded: &DecodedObligation) -> Option<u16> {
    if decoded.unhealthy_borrow_value_sf == 0 {
        return None;
    }
    let diff = decoded
        .unhealthy_borrow_value_sf
        .saturating_sub(decoded.borrowed_assets_market_value_sf);
    let bps = diff.saturating_mul(10_000) / decoded.unhealthy_borrow_value_sf;
    Some(u16::try_from(bps).unwrap_or(u16::MAX))
}

/// Compute current LTV (loan-to-value) in basis points from a decoded
/// obligation.
///
/// LTV = `borrowed_assets_market_value_sf / deposited_value_sf`. Both are
/// sf-scaled; the scaling cancels in the ratio. Returns 0 for an empty
/// position (no deposits). Result is clamped to `u16::MAX`.
///
/// Mirrors `kamino_loader::query_position_ltv_bps` but on the decoded
/// struct — used by the poller so a single `fetch_obligation` call drives
/// both LTV refresh and band classification.
pub fn compute_ltv_bps(decoded: &DecodedObligation) -> u16 {
    if decoded.deposited_value_sf == 0 {
        return 0;
    }
    let bps = decoded
        .borrowed_assets_market_value_sf
        .saturating_mul(10_000)
        .checked_div(decoded.deposited_value_sf)
        .unwrap_or(0);
    bps.min(u16::MAX as u128) as u16
}

/// Classify a position by its liquidation-distance band.
///
/// Returns `Some(severity)` if the position has crossed a band threshold,
/// `None` otherwise. Boundaries are exclusive at the upper edge:
///
/// * `bps < DISTANCE_CRITICAL_BPS`  → `Critical`
/// * `bps < DISTANCE_WARNING_BPS`   → `Warning`
/// * `bps < DISTANCE_NOTICE_BPS`    → `Notice`
/// * otherwise                      → `None`
///
/// In particular, `bps == DISTANCE_WARNING_BPS` (200) classifies as
/// `Notice`, not `Warning`.
///
/// The `_view` parameter is currently unused — accepted for API stability
/// so future callers (e.g. M9 telemetry, last-seen-staleness checks) can
/// pass position context without an API break.
#[allow(clippy::needless_pass_by_ref_mut)]
pub fn classify(_view: &PositionView, decoded: &DecodedObligation) -> Option<RiskSeverity> {
    let bps = distance_bps(decoded)?;
    if bps < DISTANCE_CRITICAL_BPS {
        return Some(RiskSeverity::Critical);
    }
    if bps < DISTANCE_WARNING_BPS {
        return Some(RiskSeverity::Warning);
    }
    if bps < DISTANCE_NOTICE_BPS {
        return Some(RiskSeverity::Notice);
    }
    None
}

// ── v0.4.15: classifier functions for the previously-stub RiskKinds ─────
//
// Pre-rc15 the `RiskKind` enum carried OracleStaleness, DeltaDrift, and
// PerpFundingSpike variants but the classifier only emitted
// LiquidationDistance — the read-only fleet half was doing less than its
// name implied. These three functions fill that gap. They're pure logic
// (no I/O, no async) and the bands are conservative starting points
// tuned against production telemetry from May-June 2026.
//
// Wiring (the inputs each classifier needs are documented per function):
//   - OracleStaleness: jupiter_perps_poller already reads Pyth-derived
//     prices; expose the feed's `last_update_unix` and call this.
//   - DeltaDrift: hedgedjlp emits `current_delta_bps` in its Reports;
//     observer.rs needs `last_delta_bps` added to PositionView before
//     this classifier can fire from the existing inbox.
//   - PerpFundingSpike: Jupiter Perps API exposes per-custody funding
//     rate; researcher's existing JLP-yield watcher is the natural host
//     (or a new watcher) — that's a separate rc.
//
// Each function is unit-tested below to pin the band boundaries.

/// OracleStaleness — Notice: feed unrefreshed for ≥ 60 s. Warning at
/// 5 min. Critical at 15 min. Pyth's pull-oracle update model on
/// Solana means anyone can submit price updates; a > 1 min gap is
/// already abnormal on mainnet during normal block production.
pub const ORACLE_STALE_NOTICE_SECS: u64 = 60;
pub const ORACLE_STALE_WARNING_SECS: u64 = 300;
pub const ORACLE_STALE_CRITICAL_SECS: u64 = 900;

/// Classify oracle staleness. `age_secs = now_unix − feed_last_update_unix`.
/// Returns `None` for fresh feeds.
pub fn classify_oracle_staleness(age_secs: u64) -> Option<RiskSeverity> {
    if age_secs >= ORACLE_STALE_CRITICAL_SECS {
        return Some(RiskSeverity::Critical);
    }
    if age_secs >= ORACLE_STALE_WARNING_SECS {
        return Some(RiskSeverity::Warning);
    }
    if age_secs >= ORACLE_STALE_NOTICE_SECS {
        return Some(RiskSeverity::Notice);
    }
    None
}

/// DeltaDrift — Notice: |delta − target| ≥ 500 bps (5 %). Warning at
/// 1500 bps (15 %). Critical at 3000 bps (30 %). The thresholds match
/// hedgedjlp's own resize-loop intent (MAX_DELTA_DRIFT_BPS region) but
/// surface drift to the operator before the daemon auto-resizes — and
/// catch the case where the resize itself fails / over-hedges (the
/// v0.4.10 bug shape: cycle 5 ran at 3.91× hedge / JLP, which by this
/// classifier reads as DeltaDrift Critical).
pub const DELTA_DRIFT_NOTICE_BPS: i32 = 500;
pub const DELTA_DRIFT_WARNING_BPS: i32 = 1_500;
pub const DELTA_DRIFT_CRITICAL_BPS: i32 = 3_000;

/// Classify delta drift. `current_bps` is the actual portfolio net long
/// fraction in bps; `target_bps` is the configured target (typically 0
/// for delta-neutral). Returns `None` for in-band drift.
pub fn classify_delta_drift(current_bps: i32, target_bps: i32) -> Option<RiskSeverity> {
    let abs_drift = (current_bps - target_bps).unsigned_abs() as i32;
    if abs_drift >= DELTA_DRIFT_CRITICAL_BPS {
        return Some(RiskSeverity::Critical);
    }
    if abs_drift >= DELTA_DRIFT_WARNING_BPS {
        return Some(RiskSeverity::Warning);
    }
    if abs_drift >= DELTA_DRIFT_NOTICE_BPS {
        return Some(RiskSeverity::Notice);
    }
    None
}

/// PerpFundingSpike — Notice at ≥ 100 bps annualised (1 %). Warning at
/// 500 bps. Critical at 2000 bps (20 % annualised — the spike that ate
/// hedgedjlp cycle 3's net APR in early May). Funding rate is signed —
/// the gate fires on |rate| so both long-pay-short and short-pay-long
/// regimes escalate.
pub const PERP_FUNDING_NOTICE_BPS_PA: i32 = 100;
pub const PERP_FUNDING_WARNING_BPS_PA: i32 = 500;
pub const PERP_FUNDING_CRITICAL_BPS_PA: i32 = 2_000;

/// Classify a perp-funding-rate spike. `funding_bps_pa` is the
/// annualised funding rate in bps; sign indicates direction (positive
/// = longs pay shorts). The classifier fires on absolute magnitude.
/// Returns `None` for in-band funding.
pub fn classify_perp_funding_spike(funding_bps_pa: i32) -> Option<RiskSeverity> {
    let abs_rate = funding_bps_pa.unsigned_abs() as i32;
    if abs_rate >= PERP_FUNDING_CRITICAL_BPS_PA {
        return Some(RiskSeverity::Critical);
    }
    if abs_rate >= PERP_FUNDING_WARNING_BPS_PA {
        return Some(RiskSeverity::Warning);
    }
    if abs_rate >= PERP_FUNDING_NOTICE_BPS_PA {
        return Some(RiskSeverity::Notice);
    }
    None
}

#[cfg(test)]
mod rc15_classifier_tests {
    use super::*;

    #[test]
    fn rc15_oracle_staleness_band_boundaries() {
        assert_eq!(classify_oracle_staleness(0), None);
        assert_eq!(classify_oracle_staleness(59), None);
        assert_eq!(classify_oracle_staleness(60), Some(RiskSeverity::Notice));
        assert_eq!(classify_oracle_staleness(299), Some(RiskSeverity::Notice));
        assert_eq!(classify_oracle_staleness(300), Some(RiskSeverity::Warning));
        assert_eq!(classify_oracle_staleness(899), Some(RiskSeverity::Warning));
        assert_eq!(classify_oracle_staleness(900), Some(RiskSeverity::Critical));
        assert_eq!(classify_oracle_staleness(86_400), Some(RiskSeverity::Critical));
    }

    #[test]
    fn rc15_delta_drift_band_boundaries() {
        // target_bps = 0 (delta-neutral).
        assert_eq!(classify_delta_drift(0, 0), None);
        assert_eq!(classify_delta_drift(499, 0), None);
        assert_eq!(classify_delta_drift(500, 0), Some(RiskSeverity::Notice));
        assert_eq!(classify_delta_drift(-500, 0), Some(RiskSeverity::Notice));
        assert_eq!(classify_delta_drift(1_499, 0), Some(RiskSeverity::Notice));
        assert_eq!(classify_delta_drift(1_500, 0), Some(RiskSeverity::Warning));
        assert_eq!(classify_delta_drift(-1_500, 0), Some(RiskSeverity::Warning));
        assert_eq!(classify_delta_drift(2_999, 0), Some(RiskSeverity::Warning));
        assert_eq!(classify_delta_drift(3_000, 0), Some(RiskSeverity::Critical));
        assert_eq!(classify_delta_drift(-30_000, 0), Some(RiskSeverity::Critical));
    }

    #[test]
    fn rc15_delta_drift_handles_nonzero_target() {
        // target_bps = +500 (small long bias intentional).
        // current 500 → drift 0 → None.
        assert_eq!(classify_delta_drift(500, 500), None);
        // current 1000 → drift 500 → Notice.
        assert_eq!(classify_delta_drift(1_000, 500), Some(RiskSeverity::Notice));
        // current -1000 → drift 1500 → Warning.
        assert_eq!(classify_delta_drift(-1_000, 500), Some(RiskSeverity::Warning));
    }

    #[test]
    fn rc15_perp_funding_band_boundaries() {
        assert_eq!(classify_perp_funding_spike(0), None);
        assert_eq!(classify_perp_funding_spike(99), None);
        assert_eq!(classify_perp_funding_spike(100), Some(RiskSeverity::Notice));
        assert_eq!(classify_perp_funding_spike(-100), Some(RiskSeverity::Notice));
        assert_eq!(classify_perp_funding_spike(499), Some(RiskSeverity::Notice));
        assert_eq!(classify_perp_funding_spike(500), Some(RiskSeverity::Warning));
        assert_eq!(classify_perp_funding_spike(-500), Some(RiskSeverity::Warning));
        assert_eq!(classify_perp_funding_spike(1_999), Some(RiskSeverity::Warning));
        assert_eq!(classify_perp_funding_spike(2_000), Some(RiskSeverity::Critical));
        assert_eq!(classify_perp_funding_spike(-2_000), Some(RiskSeverity::Critical));
    }
}
