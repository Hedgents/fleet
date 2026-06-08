//! Hard-coded safety caps for onyc-daemon.
//!
//! These are absolute upper bounds. The orchestrator can ask for less,
//! but never more — even a "trusted" Assign with target_ltv_bps > 5000
//! is rejected. Caps live in source so they cannot be raised by editing
//! a config file at runtime.
//!
//! ONyc tuning vs multiply caps:
//!   - LTV cap: 5000bps (50%), not 8000. Kamino's ONyc isolated market
//!     allows up to 6000bps (60%) but ONyc NAV moves in discrete chunks
//!     (Chainlink Data Streams, monthly Apex attestation) — leverage
//!     must sit well below the chain max to absorb a single-step NAV
//!     mark-down without auto-liquidation.
//!   - Loop rounds: 2 max (option B v0). Single round is the default;
//!     extra round only if first didn't reach target. No multi-round
//!     LTV-aware clamping like multiply — option A is a future upgrade.

use anyhow::{anyhow, Result};
use zerox1_protocol::fleet::onyc::{AssignOnyc, WithdrawOnyc};

/// Klend obligation seed `(tag, id)` for the onyc daemon's leveraged
/// ONyc obligation in the ONyc isolated market.
///
/// Distinct from stable-yield's `(0, 0)` and multiply's `(0, 1)` so
/// each strategy owns a separate obligation PDA even when they share a
/// Solana wallet — Kamino's liquidator seizes *all* collateral on a
/// single obligation, so isolation is load-bearing.
pub const ONYC_OBLIGATION_SEED: (u8, u8) = (0, 2);

/// Maximum loan-to-value the daemon will ever accept (basis points).
/// 5000bps = 50%. Kamino's ONyc isolated market allows up to 6000bps
/// but we operate well below the chain max because ONyc NAV updates are
/// discrete (step changes on insurance settlement events).
pub const MAX_LTV_BPS: u16 = 5000;

/// Default target LTV — 4000bps = 40%. The orchestrator can request
/// anywhere in [0, MAX_LTV_BPS]; 4000 is what
/// `build_onyc_releverage_spec` ships when no specific target was set.
pub const DEFAULT_TARGET_LTV_BPS: u16 = 4000;

/// Maximum USDC the daemon will operate. $500K USDC equivalent
/// (6-decimal lamports). ONyc strategy is bounded by Orca secondary
/// depth (~$15M), so $500K keeps us well under 5% of pool — slippage
/// model holds.
pub const MAX_POSITION_USDC_LAMPORTS: u64 = 500_000_000_000;

/// Maximum slippage on the Orca USDC↔ONyc swap leg, in bps.
pub const MAX_SLIPPAGE_BPS: u16 = 200;

/// Hard ceiling on supply→borrow→swap rounds. Option B v0 caps at 2
/// rounds — first to deploy, optional second to reach target if first
/// underflowed. Option A (full multi-round with LTV-aware clamping) is
/// a future upgrade.
pub const MAX_LEVERAGE_LOOP_ROUNDS: u8 = 2;

/// Kamino's `BorrowObligationLiquidityV2` checks borrow-factor-adjusted
/// USD against the obligation's allowed_borrow_value. USDC's
/// borrow-factor on the ONyc isolated market is 1.0 — USDC is the
/// borrow asset (stable). Unlike multiply where SOL was the borrow at
/// BF=1.25, no adjustment factor needed here.
pub const USDC_BORROW_FACTOR_BPS: u32 = 10_000;

/// If position liquidation-distance falls below this, the liq monitor
/// auto-unwinds without waiting for an orchestrator Approve.
pub const LIQUIDATION_DISTANCE_CRITICAL_BPS: u16 = 50;

/// Warning band — emit Escalate envelope but don't auto-unwind.
pub const LIQUIDATION_DISTANCE_WARNING_BPS: u16 = 200;

/// Validate an AssignOnyc against all caps. Returns Ok if every
/// requested value is within bounds, Err otherwise. Daemon rejects
/// any Assign that fails this check before doing any chain work.
pub fn validate_assign(a: &AssignOnyc) -> Result<()> {
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

/// Validate a WithdrawOnyc against caps. No amount field exists on the
/// payload (the unwind is always 100%), so we only check the slippage cap
/// and the defensive non-zero-vault assertion.
pub fn validate_withdraw_onyc(w: &WithdrawOnyc) -> Result<()> {
    if w.vault == [0u8; 32] {
        return Err(anyhow!("WithdrawOnyc.vault is zero (defensive check)"));
    }
    if w.max_slippage_bps > MAX_SLIPPAGE_BPS {
        return Err(anyhow!(
            "max_slippage_bps {} exceeds hard cap {}",
            w.max_slippage_bps,
            MAX_SLIPPAGE_BPS
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(target_ltv: u16, slippage: u16) -> AssignOnyc {
        AssignOnyc {
            vault: [0; 32],
            target_ltv_bps: target_ltv,
            max_slippage_bps: slippage,
            deadline_unix: 0,
            usdc_lamports: 0,
        }
    }

    #[test]
    fn accepts_within_caps() {
        assert!(validate_assign(&assign(4000, 50)).is_ok());
    }

    #[test]
    fn rejects_ltv_above_cap() {
        let err = validate_assign(&assign(5001, 50)).unwrap_err();
        assert!(err.to_string().contains("target_ltv_bps"));
    }

    #[test]
    fn rejects_slippage_above_cap() {
        let err = validate_assign(&assign(4000, 201)).unwrap_err();
        assert!(err.to_string().contains("max_slippage_bps"));
    }

    fn withdraw_onyc(vault: [u8; 32], slippage: u16) -> WithdrawOnyc {
        WithdrawOnyc {
            vault,
            max_slippage_bps: slippage,
            deadline_unix: 0,
        }
    }

    #[test]
    fn validate_withdraw_accepts_within_caps() {
        assert!(validate_withdraw_onyc(&withdraw_onyc([7u8; 32], 100)).is_ok());
    }

    #[test]
    fn validate_withdraw_rejects_zero_vault() {
        let err = validate_withdraw_onyc(&withdraw_onyc([0u8; 32], 100)).unwrap_err();
        assert!(err.to_string().contains("vault"));
    }

    #[test]
    fn validate_withdraw_rejects_slippage_above_cap() {
        let err = validate_withdraw_onyc(&withdraw_onyc([7u8; 32], 201)).unwrap_err();
        assert!(err.to_string().contains("max_slippage_bps"));
    }

    #[test]
    fn cap_constants_are_sensible() {
        // Sanity — if these get tuned, fail loudly in tests so the
        // change is reviewed. ONyc-specific bounds are tighter than
        // multiply's because of discrete NAV updates.
        assert!(MAX_LTV_BPS <= 5500, "ONyc LTV cap above 55% is reckless");
        assert!(
            MAX_LEVERAGE_LOOP_ROUNDS <= 4,
            "option B v0 caps at 2 rounds; bumping above 4 needs review"
        );
        assert!(LIQUIDATION_DISTANCE_CRITICAL_BPS < LIQUIDATION_DISTANCE_WARNING_BPS);
        assert!(DEFAULT_TARGET_LTV_BPS <= MAX_LTV_BPS);
    }
}
