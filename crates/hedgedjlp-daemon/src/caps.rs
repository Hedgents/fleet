//! Hard-coded safety caps for hedgedjlp-daemon.
//!
//! Compile-time absolute upper bounds. Cannot be raised at runtime.
//! HedgedJLP runs two legs: a JLP buy and Jupiter Perps shorts. Caps
//! bound (a) total capital deployed, (b) acceptable delta drift before
//! emergency rebalance, (c) borrow-rate ceiling above which the daemon
//! refuses to operate, (d) hedge-leg leverage ceiling.
//!
//! No funding-rate cap (Jupiter Perps has no funding — only borrow fees
//! that scale with utilization under Gauntlet's jump-rate model).

use anyhow::{anyhow, Result};
use zerox1_protocol::fleet::hedgedjlp::AssignHedgedJlp;

/// Maximum total USDC the daemon will deploy across both legs. $5M.
/// (USDC has 6 decimals → 5_000_000_000_000 = $5M.)
pub const MAX_POSITION_USDC_LAMPORTS: u64 = 5_000_000_000_000;

/// Minimum position size — JLP buy + Jupiter Perps account init + at
/// least 1 short request has fixed costs (rent + tx fees + Jupiter
/// keeper compensation). Below $100 the strategy doesn't pencil.
pub const MIN_POSITION_USDC_LAMPORTS: u64 = 100_000_000;  // $100

/// Maximum allowed delta drift in basis points — beyond this, the
/// rebalancer triggers emergency hedge resize. ±10%.
/// AssignHedgedJlp.target_delta_bps is bounded to this range.
pub const MAX_DELTA_DRIFT_BPS: u16 = 1000;

/// Hard ceiling on Jupiter Perps borrow-rate the daemon will tolerate.
/// 50% APR — at this rate the basis trade has zero net (typical JLP
/// yield is 30-50% APR, so 50% borrow eats it entirely). Above this,
/// the auto-unwind path triggers regardless of orchestrator config.
pub const MAX_BORROW_RATE_BPS_HARDCAP: u16 = 5000;

/// Hedge-leg leverage cap — Jupiter Perps program enforces ≤50x; we
/// cap much lower for blast-radius bounding. 3x means a 33% adverse
/// SOL move would liquidate (vs ~2% on 50x).
pub const MAX_LEVERAGE_ON_HEDGE: u8 = 3;

/// Validate an AssignHedgedJlp against the hard caps. Refuses any
/// out-of-bounds field before chain work.
pub fn validate_assign(a: &AssignHedgedJlp) -> Result<()> {
    if a.usdc_lamports > MAX_POSITION_USDC_LAMPORTS {
        return Err(anyhow!(
            "usdc_lamports {} exceeds hard cap {}",
            a.usdc_lamports,
            MAX_POSITION_USDC_LAMPORTS
        ));
    }
    if a.usdc_lamports < MIN_POSITION_USDC_LAMPORTS {
        return Err(anyhow!(
            "usdc_lamports {} below minimum {} (sub-$100 doesn't pencil after fixed costs)",
            a.usdc_lamports,
            MIN_POSITION_USDC_LAMPORTS
        ));
    }
    let abs_delta = a.target_delta_bps.unsigned_abs();
    if abs_delta > MAX_DELTA_DRIFT_BPS {
        return Err(anyhow!(
            "target_delta_bps {} exceeds ±{} hard cap",
            a.target_delta_bps,
            MAX_DELTA_DRIFT_BPS
        ));
    }
    if a.max_borrow_rate_bps > MAX_BORROW_RATE_BPS_HARDCAP {
        return Err(anyhow!(
            "max_borrow_rate_bps {} exceeds hard cap {}",
            a.max_borrow_rate_bps,
            MAX_BORROW_RATE_BPS_HARDCAP
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(usdc: u64, delta_bps: i16, borrow_bps: u16) -> AssignHedgedJlp {
        AssignHedgedJlp {
            usdc_lamports: usdc,
            target_delta_bps: delta_bps,
            max_borrow_rate_bps: borrow_bps,
            deadline_unix: 0,
        }
    }

    #[test]
    fn accepts_within_bounds() {
        assert!(validate_assign(&assign(200_000_000, 0, 3000)).is_ok());
    }

    #[test]
    fn rejects_above_max_position() {
        let err = validate_assign(&assign(MAX_POSITION_USDC_LAMPORTS + 1, 0, 3000)).unwrap_err();
        assert!(err.to_string().contains("exceeds hard cap"));
    }

    #[test]
    fn rejects_below_min_position() {
        let err = validate_assign(&assign(MIN_POSITION_USDC_LAMPORTS - 1, 0, 3000)).unwrap_err();
        assert!(err.to_string().contains("doesn't pencil"));
    }

    #[test]
    fn rejects_excessive_positive_delta() {
        let err = validate_assign(&assign(200_000_000, 1001, 3000)).unwrap_err();
        assert!(err.to_string().contains("target_delta_bps"));
    }

    #[test]
    fn rejects_excessive_negative_delta() {
        let err = validate_assign(&assign(200_000_000, -1001, 3000)).unwrap_err();
        assert!(err.to_string().contains("target_delta_bps"));
    }

    #[test]
    fn accepts_boundary_negative_delta() {
        assert!(validate_assign(&assign(200_000_000, -1000, 3000)).is_ok());
    }

    #[test]
    fn rejects_borrow_above_hardcap() {
        let err = validate_assign(&assign(200_000_000, 0, MAX_BORROW_RATE_BPS_HARDCAP + 1)).unwrap_err();
        assert!(err.to_string().contains("max_borrow_rate_bps"));
    }

    #[test]
    fn cap_constants_are_sensible() {
        assert!(MAX_POSITION_USDC_LAMPORTS >= MIN_POSITION_USDC_LAMPORTS * 100, "max should dwarf min");
        assert!(MAX_DELTA_DRIFT_BPS <= 2000, "more than 20% drift defeats the purpose of delta-neutral");
        assert!(MAX_BORROW_RATE_BPS_HARDCAP <= 10000, "above 100% APR borrow is absurd");
        assert!(MAX_LEVERAGE_ON_HEDGE <= 5, "anything above 5x on hedge has thin liq buffer");
    }
}
