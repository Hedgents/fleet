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
