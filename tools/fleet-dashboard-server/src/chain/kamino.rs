//! Kamino obligation reads for the dashboard.
//!
//! Wraps `zerox1_defi_protocols::protocols::kamino_loader::fetch_obligation`
//! into two best-effort views:
//! - `ObligationView` for the multiply daemon (deposited + borrowed +
//!   computed LTV in bps).
//! - `SupplyView` for the stable-yield daemon (deposited cTokens converted
//!   to USDC lamports via the reserve's live exchange rate, not the
//!   obligation's stale `market_value_sf`).
//!
//! Both readers return `Ok(None)` if the obligation account doesn't exist
//! yet (fresh wallet) — this keeps the dashboard responsive while the
//! operator is still funding.

use anyhow::Result;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use zerox1_defi_protocols::protocols::kamino;
use zerox1_defi_protocols::protocols::kamino_loader;
use zerox1_defi_protocols::protocols::kamino_loader::DecodedObligation;

/// Multiply's obligation view: deposited collateral, debt, current LTV.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ObligationView {
    pub obligation_pubkey: Pubkey,
    pub ltv_bps: u16,
    /// Deposited collateral USD value, micro-units (1e-6 USD).
    pub deposited_usd_micro: u64,
    /// Borrowed assets USD value, micro-units.
    pub borrowed_usd_micro: u64,
}

/// Stable-yield's supply view: deposited USDC into a Kamino reserve.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SupplyView {
    pub reserve_pubkey: Pubkey,
    /// Deposited USDC in lamports (1e-6 USDC, 6 decimals).
    ///
    /// This is the underlying USDC value, derived as
    /// `ctokens × (total_liquidity / collateral_mint_total_supply)` using the
    /// live reserve fields. We do **not** read the obligation's
    /// `market_value_sf` because that field is only recomputed during
    /// `RefreshObligation`; our deposit bundle order `[Refresh, Deposit]`
    /// causes it to be permanently stale by exactly the round's deposit
    /// amount, which produced a ~10× under-report in v0.1.7.
    pub deposited_usdc_lamports: u64,
}

/// Read multiply's obligation. `payer` is the operator wallet, `market` is
/// the Kamino main lending market PDA.
pub async fn read_multiply_obligation(
    rpc: &RpcClient,
    payer: &Pubkey,
    market: &Pubkey,
) -> Result<Option<ObligationView>> {
    let obligation_pk = kamino::derive_user_obligation(payer, market);
    let Some(decoded) = kamino_loader::fetch_obligation(rpc, &obligation_pk).await? else {
        return Ok(None);
    };
    Ok(multiply_view_from_obligation(obligation_pk, &decoded))
}

/// Pure decision: given a decoded obligation, return the multiply view, or
/// `None` if the obligation does not represent a multiply position.
///
/// The fleet shares a single operator wallet across all yield daemons,
/// which means the multiply PDA and the stable-yield PDA are the same
/// account. Stable-yield only deposits USDC (no borrow); multiply by
/// construction always has an open borrow. We discriminate on the
/// presence of at least one open borrow — without this guard, the
/// dashboard misattributes stable-yield's deposit to multiply (Bug 1).
pub fn multiply_view_from_obligation(
    obligation_pk: Pubkey,
    decoded: &DecodedObligation,
) -> Option<ObligationView> {
    if decoded.borrows.is_empty() {
        return None;
    }
    let ltv_bps = if decoded.deposited_value_sf == 0 {
        0u16
    } else {
        let ratio = decoded
            .borrowed_assets_market_value_sf
            .saturating_mul(10_000)
            .checked_div(decoded.deposited_value_sf)
            .unwrap_or(0);
        ratio.min(u16::MAX as u128) as u16
    };
    // sf-scaled USD: divide by 2^60 to get USD; we render in micro-USD
    // (1e-6 USD) so multiply by 1e6 then shift by 60.
    let deposited_usd_micro = sf_to_micro_usd(decoded.deposited_value_sf);
    let borrowed_usd_micro = sf_to_micro_usd(decoded.borrowed_assets_market_value_sf);
    Some(ObligationView {
        obligation_pubkey: obligation_pk,
        ltv_bps,
        deposited_usd_micro,
        borrowed_usd_micro,
    })
}

/// Read stable-yield's supply view. We surface the deposited USDC (in
/// lamports) found on the obligation against the named reserve.
///
/// Returns `Ok(None)` if no obligation exists or no deposit against
/// `reserve` is present.
pub async fn read_stable_yield_supply(
    rpc: &RpcClient,
    payer: &Pubkey,
    market: &Pubkey,
    reserve: &Pubkey,
) -> Result<Option<SupplyView>> {
    let obligation_pk = kamino::derive_user_obligation(payer, market);
    let Some(decoded) = kamino_loader::fetch_obligation(rpc, &obligation_pk).await? else {
        return Ok(None);
    };
    let Some(deposit) = decoded.deposits.iter().find(|d| &d.reserve == reserve) else {
        return Ok(None);
    };
    let reserve_liq = kamino_loader::fetch_reserve_liquidity(rpc, reserve).await?;
    Ok(Some(supply_view_from_deposit(
        *reserve,
        deposit,
        &reserve_liq,
    )))
}

/// Pure conversion: obligation deposit slot + reserve numerics → SupplyView.
///
/// Computes the underlying USDC value as
/// `ctokens × (total_liquidity / collateral_mint_total_supply)` directly
/// from the live reserve account. We do **not** read `deposit.market_value_sf`
/// because Kamino only updates that field during `RefreshObligation`; with
/// our `[Refresh, Deposit]` bundle order it is permanently stale (Bug 2 in
/// v0.1.7 — surfaced an off-by-~10× under-report on a $54 position).
pub fn supply_view_from_deposit(
    reserve: Pubkey,
    deposit: &kamino_loader::ObligationDeposit,
    reserve_liq: &kamino_loader::DecodedReserveLiquidity,
) -> SupplyView {
    SupplyView {
        reserve_pubkey: reserve,
        deposited_usdc_lamports: reserve_liq.ctokens_to_liquidity(deposit.deposited_amount),
    }
}

fn sf_to_micro_usd(sf: u128) -> u64 {
    // sf is value * 2^60. We want value * 1_000_000.
    // value_micro = sf * 1_000_000 / 2^60 = sf >> 60 * 1_000_000 (lossy
    // for high precision but fine for dashboard display).
    let usd = (sf >> 60) as u128;
    let micro = usd.saturating_mul(1_000_000);
    micro.min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerox1_defi_protocols::protocols::kamino_loader::{
        DecodedReserveLiquidity, ObligationBorrow, ObligationDeposit,
    };

    /// Build a `DecodedReserveLiquidity` whose exchange rate equals
    /// `target` (liquidity-per-cToken). Picks a cToken supply of 1e9 and
    /// sets available_amount to `target × supply`, with no outstanding
    /// borrows.
    fn reserve_with_rate(target_ratio: f64) -> DecodedReserveLiquidity {
        let supply: u64 = 1_000_000_000;
        let avail = (target_ratio * supply as f64).round() as u64;
        DecodedReserveLiquidity {
            available_amount: avail,
            borrowed_amount_sf: 0,
            collateral_mint_total_supply: supply,
        }
    }

    fn obligation_with(
        deposits: Vec<ObligationDeposit>,
        borrows: Vec<ObligationBorrow>,
        deposited_value_sf: u128,
        borrowed_market_value_sf: u128,
    ) -> DecodedObligation {
        DecodedObligation {
            address: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            deposits,
            borrows,
            deposited_value_sf,
            borrow_factor_adjusted_debt_value_sf: 0,
            borrowed_assets_market_value_sf: borrowed_market_value_sf,
            allowed_borrow_value_sf: 0,
            unhealthy_borrow_value_sf: 0,
        }
    }

    #[test]
    fn multiply_view_none_when_no_borrows() {
        // Stable-yield-style: deposit exists, no borrows.
        // The shared obligation must NOT be attributed to multiply.
        let deposit = ObligationDeposit {
            reserve: Pubkey::new_unique(),
            deposited_amount: 5_000_000,
            market_value_sf: 5u128 << 60,
        };
        let o = obligation_with(vec![deposit], vec![], 5u128 << 60, 0);
        let view = multiply_view_from_obligation(Pubkey::new_unique(), &o);
        assert!(
            view.is_none(),
            "multiply must not claim a stable-yield-only obligation"
        );
    }

    #[test]
    fn multiply_view_some_when_borrow_exists() {
        let deposit = ObligationDeposit {
            reserve: Pubkey::new_unique(),
            deposited_amount: 100_000_000,
            market_value_sf: 100u128 << 60,
        };
        let borrow = ObligationBorrow {
            reserve: Pubkey::new_unique(),
            borrowed_amount_sf: 50u128 << 60,
            market_value_sf: 50u128 << 60,
            borrow_factor_adjusted_market_value_sf: 50u128 << 60,
        };
        let o = obligation_with(vec![deposit], vec![borrow], 100u128 << 60, 50u128 << 60);
        let view = multiply_view_from_obligation(Pubkey::new_unique(), &o)
            .expect("multiply with a borrow should produce a view");
        assert_eq!(view.deposited_usd_micro, 100_000_000);
        assert_eq!(view.borrowed_usd_micro, 50_000_000);
        assert_eq!(view.ltv_bps, 5000); // 50/100 = 5000 bps
    }

    #[test]
    fn supply_view_uses_reserve_exchange_rate_not_market_value_sf() {
        // Bug 2 (v0.1.7) regression: on the live BPEv2... obligation,
        // deposited_amount = 46_562_924 cTokens reflects $54.23 of USDC, but
        // market_value_sf was frozen at $5 because RefreshObligation runs
        // before Deposit in our bundle. The fix computes USDC value from the
        // live reserve exchange rate (1.165× at the time of the report)
        // rather than reading the stale obligation field.
        let reserve = Pubkey::new_unique();
        let reserve_liq = reserve_with_rate(1.165);
        let deposit = ObligationDeposit {
            reserve,
            deposited_amount: 46_562_924,
            // Deliberately wrong-shaped — proves we ignore market_value_sf:
            market_value_sf: 5u128 << 60,
        };
        let view = supply_view_from_deposit(reserve, &deposit, &reserve_liq);
        // 46_562_924 × 1.165 ≈ 54_245_806 lamports; allow ±100k tolerance.
        let expected = 54_230_000_i64;
        let got = view.deposited_usdc_lamports as i64;
        assert!(
            (got - expected).abs() < 100_000,
            "expected ~{expected} ± 100k lamports, got {got}"
        );
        assert!(
            got > 50_000_000 && got < 60_000_000,
            "USDC value must reflect $54-ish, not the $5 stale market_value_sf or the raw 46M cToken count"
        );
    }

    #[test]
    fn supply_view_handles_borrowed_liquidity_in_total() {
        // total_liquidity = available + (borrowed_sf >> 60). With 500M
        // available, 500M borrowed-as-sf, and 1B cToken supply, the
        // exchange rate is exactly 1.0× — depositors haven't accrued yet.
        let reserve = Pubkey::new_unique();
        let reserve_liq = DecodedReserveLiquidity {
            available_amount: 500_000_000,
            borrowed_amount_sf: 500_000_000u128 << 60,
            collateral_mint_total_supply: 1_000_000_000,
        };
        let deposit = ObligationDeposit {
            reserve,
            deposited_amount: 100_000_000,
            market_value_sf: 0, // ignored
        };
        let view = supply_view_from_deposit(reserve, &deposit, &reserve_liq);
        assert_eq!(view.deposited_usdc_lamports, 100_000_000);
    }

    #[test]
    fn supply_view_zero_when_mint_supply_zero() {
        let reserve = Pubkey::new_unique();
        let reserve_liq = DecodedReserveLiquidity {
            available_amount: 0,
            borrowed_amount_sf: 0,
            collateral_mint_total_supply: 0,
        };
        let deposit = ObligationDeposit {
            reserve,
            deposited_amount: 1_000_000,
            market_value_sf: 0,
        };
        let view = supply_view_from_deposit(reserve, &deposit, &reserve_liq);
        assert_eq!(view.deposited_usdc_lamports, 0);
    }

    #[test]
    fn multiply_view_zero_ltv_when_deposit_value_zero() {
        let borrow = ObligationBorrow {
            reserve: Pubkey::new_unique(),
            borrowed_amount_sf: 1u128 << 60,
            market_value_sf: 1u128 << 60,
            borrow_factor_adjusted_market_value_sf: 1u128 << 60,
        };
        let o = obligation_with(vec![], vec![borrow], 0, 1u128 << 60);
        let view = multiply_view_from_obligation(Pubkey::new_unique(), &o).unwrap();
        assert_eq!(view.ltv_bps, 0);
    }
}
