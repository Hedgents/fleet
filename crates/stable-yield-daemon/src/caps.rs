//! Hard-coded safety caps for stable-yield-daemon.
//!
//! Compile-time absolute upper bounds. Cannot be raised by editing a config
//! file — caps live in source so the only way to lift them is to change
//! the binary, which is itself a review-gated event.
//!
//! Stable-yield is simpler than multiply: no leverage, no swap, no LTV.
//! The only operational cap is position size.

use anyhow::{anyhow, Result};
use zerox1_protocol::fleet::stable_lend::AssignStableLend;

/// Maximum total USDC the daemon will supply. $5M USDC equivalent
/// (USDC has 6 decimals → 5_000_000_000_000 = 5e6 USDC).
/// Bounds blast radius if orchestrator keys are compromised.
pub const MAX_POSITION_USDC_LAMPORTS: u64 = 5_000_000_000_000;

/// Minimum position size — refuses dust deposits that cost more in
/// rent + tx fees than they earn. $1 USDC.
pub const MIN_POSITION_USDC_LAMPORTS: u64 = 1_000_000;

/// Validate an AssignStableLend against the hard caps. Returns Ok if
/// `usdc_lamports` is within bounds, Err otherwise. Daemon rejects any
/// Assign that fails this check before doing chain work.
pub fn validate_assign(a: &AssignStableLend) -> Result<()> {
    if a.usdc_lamports > MAX_POSITION_USDC_LAMPORTS {
        return Err(anyhow!(
            "usdc_lamports {} exceeds hard cap {}",
            a.usdc_lamports,
            MAX_POSITION_USDC_LAMPORTS
        ));
    }
    if a.usdc_lamports < MIN_POSITION_USDC_LAMPORTS {
        return Err(anyhow!(
            "usdc_lamports {} below minimum {} (dust-deposit guard)",
            a.usdc_lamports,
            MIN_POSITION_USDC_LAMPORTS
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(amount: u64) -> AssignStableLend {
        AssignStableLend {
            market: [0; 32],
            reserve: [0; 32],
            usdc_lamports: amount,
            deadline_unix: 0,
        }
    }

    #[test]
    fn accepts_within_bounds() {
        assert!(validate_assign(&assign(100_000_000)).is_ok()); // $100
    }

    #[test]
    fn rejects_above_max() {
        let err = validate_assign(&assign(MAX_POSITION_USDC_LAMPORTS + 1)).unwrap_err();
        assert!(err.to_string().contains("exceeds hard cap"));
    }

    #[test]
    fn rejects_below_min() {
        let err = validate_assign(&assign(MIN_POSITION_USDC_LAMPORTS - 1)).unwrap_err();
        assert!(err.to_string().contains("dust-deposit guard"));
    }

    #[test]
    fn boundary_inclusive_min() {
        assert!(validate_assign(&assign(MIN_POSITION_USDC_LAMPORTS)).is_ok());
    }

    #[test]
    fn boundary_inclusive_max() {
        assert!(validate_assign(&assign(MAX_POSITION_USDC_LAMPORTS)).is_ok());
    }

    #[test]
    fn cap_constants_are_sensible() {
        assert!(MAX_POSITION_USDC_LAMPORTS <= 10_000_000_000_000, "more than $10M is reckless v0 cap");
        assert!(MIN_POSITION_USDC_LAMPORTS >= 1_000_000, "less than $1 is dust");
        assert!(MIN_POSITION_USDC_LAMPORTS < MAX_POSITION_USDC_LAMPORTS);
    }
}
