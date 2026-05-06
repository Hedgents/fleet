//! Hard-coded safety caps for stable-yield-daemon.
//!
//! Compile-time absolute upper bounds. Cannot be raised by editing a config
//! file — caps live in source so the only way to lift them is to change
//! the binary, which is itself a review-gated event.
//!
//! Stable-yield is simpler than multiply: no leverage, no swap, no LTV.
//! The only operational cap is position size.

use anyhow::{anyhow, Result};
use zerox1_protocol::fleet::stable_lend::{AssignStableLend, WithdrawStableLend};

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

/// Validate a WithdrawStableLend. Refuses zero amounts (nonsensical
/// withdraw) but accepts `u64::MAX` as the "withdraw all" sentinel.
///
/// Withdrawal needs minimal caps — you can't over-withdraw (the protocol
/// caps at deposited amount), so the only sanity check is "non-zero amount
/// or u64::MAX sentinel".
pub fn validate_withdraw(w: &WithdrawStableLend) -> Result<()> {
    if w.usdc_lamports == 0 {
        return Err(anyhow!(
            "withdraw usdc_lamports must be > 0 (or u64::MAX for full)"
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

    fn withdraw(amount: u64) -> WithdrawStableLend {
        WithdrawStableLend {
            market: [0; 32],
            reserve: [0; 32],
            usdc_lamports: amount,
            deadline_unix: 0,
        }
    }

    #[test]
    fn withdraw_rejects_zero() {
        let err = validate_withdraw(&withdraw(0)).unwrap_err();
        assert!(err.to_string().contains("must be > 0"));
    }

    #[test]
    fn withdraw_accepts_max() {
        assert!(validate_withdraw(&withdraw(u64::MAX)).is_ok());
    }

    #[test]
    fn withdraw_accepts_nonzero_amount() {
        // Unlike Assign, withdraw has no min/max cap — the protocol caps it
        // at the obligation's deposited amount.
        assert!(validate_withdraw(&withdraw(1)).is_ok());
        assert!(validate_withdraw(&withdraw(50_000_000)).is_ok());
        // Even an absurdly large value passes the cap check — klend will
        // reject it on chain.
        assert!(validate_withdraw(&withdraw(u64::MAX - 1)).is_ok());
    }
}
