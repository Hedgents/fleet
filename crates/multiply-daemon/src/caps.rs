//! Hard-coded safety caps for multiply-daemon.
//!
//! These are absolute upper bounds. The orchestrator can ask for less,
//! but never more — even a "trusted" Assign with target_ltv_bps > 8000
//! is rejected. Caps live in source so they cannot be raised by editing
//! a config file at runtime.

use anyhow::{anyhow, Result};
use zerox1_protocol::fleet::multiply::AssignMultiply;

/// Maximum loan-to-value the daemon will ever accept (basis points).
/// 80% — anything higher is liquidation-bait.
pub const MAX_LTV_BPS: u16 = 8000;

/// Maximum collateral the daemon will operate. $5M USDC equivalent
/// (USDC has 6 decimals, so 5_000_000_000_000 = 5e6 USDC).
/// Keeps blast radius bounded even if the orchestrator's keys are
/// compromised.
pub const MAX_POSITION_USDC_LAMPORTS: u64 = 5_000_000_000_000;

/// Maximum slippage on the swap leg of the leverage loop, in bps.
pub const MAX_SLIPPAGE_BPS: u16 = 200;

/// Hard ceiling on supply→borrow→swap rounds. Prevents accidental
/// infinite loops if LTV math diverges.
pub const MAX_LEVERAGE_LOOP_ROUNDS: u8 = 6;

/// If position liquidation-distance falls below this, the liq monitor
/// auto-unwinds without waiting for an orchestrator Approve.
pub const LIQUIDATION_DISTANCE_CRITICAL_BPS: u16 = 50;

/// Warning band — emit Escalate envelope but don't auto-unwind.
pub const LIQUIDATION_DISTANCE_WARNING_BPS: u16 = 200;

/// Validate an AssignMultiply against all caps. Returns Ok if every
/// requested value is within bounds, Err otherwise. Daemon rejects
/// any Assign that fails this check before doing any chain work.
pub fn validate_assign(a: &AssignMultiply) -> Result<()> {
    if a.target_ltv_bps > MAX_LTV_BPS {
        return Err(anyhow!(
            "target_ltv_bps {} exceeds hard cap {}",
            a.target_ltv_bps,
            MAX_LTV_BPS
        ));
    }
    if a.max_slippage_bps > MAX_SLIPPAGE_BPS {
        return Err(anyhow!(
            "max_slippage_bps {} exceeds hard cap {}",
            a.max_slippage_bps,
            MAX_SLIPPAGE_BPS
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(target_ltv: u16, slippage: u16) -> AssignMultiply {
        AssignMultiply {
            vault: [0; 32],
            target_ltv_bps: target_ltv,
            max_slippage_bps: slippage,
            deadline_unix: 0,
        }
    }

    #[test]
    fn accepts_within_caps() {
        assert!(validate_assign(&assign(6000, 50)).is_ok());
    }

    #[test]
    fn rejects_ltv_above_cap() {
        let err = validate_assign(&assign(8001, 50)).unwrap_err();
        assert!(err.to_string().contains("target_ltv_bps"));
    }

    #[test]
    fn rejects_slippage_above_cap() {
        let err = validate_assign(&assign(6000, 201)).unwrap_err();
        assert!(err.to_string().contains("max_slippage_bps"));
    }

    #[test]
    fn cap_constants_are_sensible() {
        // Sanity — if these get tuned, fail loudly in tests so the
        // change is reviewed.
        assert!(MAX_LTV_BPS <= 8500, "LTV cap above 85% is reckless");
        assert!(MAX_LEVERAGE_LOOP_ROUNDS <= 8, "more rounds = more failure surface");
        assert!(LIQUIDATION_DISTANCE_CRITICAL_BPS < LIQUIDATION_DISTANCE_WARNING_BPS);
    }
}
