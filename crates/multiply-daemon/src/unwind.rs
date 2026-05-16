//! Lever-down unwind for multiply-daemon (commit 4 of the multiply-unwind plan).
//!
//! Pure round-builders + strategy decision. No RPC, no broadcasting — that
//! lands in commit 5 (dispatch.rs wire-up) which calls into these builders.
//!
//! Two strategies (decided per-position by [`decide_unwind_strategy`]):
//!
//! 1. **FlashLoan (preferred)** — single-tx flash-loan unwind, mirrors
//!    klend-sdk's `buildWithdrawWithLeverageIxs`:
//!      1. flash-borrow total SOL debt from Kamino's SOL reserve
//!      2. RepayObligationLiquidityV2 against the obligation
//!      3. WithdrawObligationCollateralAndRedeemReserveCollateralV2(jitoSOL)
//!      4. swap jitoSOL → SOL (Jupiter swap ixns supplied by the caller,
//!         or Jito stake-pool WithdrawSol fallback)
//!      5. FlashRepayReserveLiquidity (amount + flash fee)
//!      6. CloseAccount(wSOL ATA)
//!    Atomic — any leg failure reverts the entire tx, so the obligation
//!    never lands in a mid-unwind state where a SOL pump could push LTV
//!    above the liquidation threshold.
//!
//! 2. **Iterative (fallback)** — N rounds of bounded δ-withdraw without
//!    flash loans, used when the SOL-reserve flash cap is below total debt
//!    OR Jupiter quote unavailable at size. Each round:
//!      1. WithdrawObligationCollateralAndRedeemReserveCollateralV2(δ jitoSOL)
//!      2. jito_stake_pool::WithdrawSol(δ jitoSOL → δ SOL)
//!      3. RepayObligationLiquidityV2(δ SOL)
//!    δ sized so post-withdraw LTV stays strictly below liquidation
//!    threshold (caller-side responsibility — see §6 of the plan doc).
//!
//! Both builders are pure: they take pre-loaded `ReserveAccounts` + decoded
//! obligation state and return a `Vec<Instruction>`. RPC calls live in the
//! caller (commit 5).

use anyhow::{anyhow, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use spl_token::instruction::close_account;
use zerox1_defi_protocols::{
    constants::{JITOSOL_MINT, TOKEN_PROGRAM_ID, WSOL_MINT},
    protocols::{
        kamino::{
            flash_borrow_reserve_liquidity_ix, flash_repay_reserve_liquidity_ix,
            refresh_obligation_ix, refresh_reserve_ix, repay_obligation_liquidity_v2_ix,
            withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix, ReserveAccounts,
        },
        kamino_loader::DecodedObligation,
    },
};

use crate::caps;

/// Compute budget for the single-tx flash-loan unwind. Larger than a
/// lever-up round (which is 1M CU) because the unwind tx packs more ixns:
/// 2 compute-budget + 2 ATA-create + flash-borrow + 3 refreshes + repay +
/// 3 refreshes + withdraw + ~8 Jupiter swap + flash-repay + close = 22-25
/// ixns. The plan caps this at 1.4M CU.
pub const UNWIND_CU_LIMIT: u32 = 1_400_000;

/// Priority fee for the unwind tx, in microlamports. Mirrors lever-up.
pub const UNWIND_PRIORITY_FEE: u64 = 10_000;

/// Decision returned by [`decide_unwind_strategy`]: pure function over
/// position + reserve state.
#[derive(Debug, PartialEq, Eq)]
pub enum UnwindStrategy {
    /// Single-tx flash-loan unwind. Preferred when:
    ///   * Kamino SOL reserve flash-loan cap >= debt
    ///   * Jupiter quote available within slippage cap
    FlashLoan {
        /// Amount of SOL lamports to flash-borrow (= total debt + safety
        /// buffer to absorb the next accrued-interest tick).
        flash_amount_lamports: u64,
    },
    /// Iterative fallback. Used when flash cap < debt OR Jupiter quote
    /// unavailable. Caller runs N rounds of bounded δ-withdraw.
    Iterative {
        /// Total number of rounds the caller should execute. Capped at
        /// [`caps::MAX_LEVERAGE_LOOP_ROUNDS`].
        rounds: u8,
    },
    /// Nothing to unwind — obligation has no SOL borrow AND no jitoSOL
    /// collateral. Caller returns ok=true with `final_usdc_lamports=0`,
    /// not an error.
    Noop,
}

/// Pure decision over (obligation, reserves, flash availability).
///
/// Inputs:
/// * `obligation` — decoded obligation state (deposits + borrows).
/// * `sol_reserve_pubkey`, `jitosol_reserve_pubkey` — to filter the
///   obligation's deposits/borrows arrays.
/// * `sol_reserve_flash_cap_lamports` — the SOL reserve's
///   `liquidity.available_amount`. If this is below the position's debt,
///   we cannot flash-borrow the full amount and must fall back to iterative.
/// * `jupiter_quote_available_and_in_slippage` — caller passes `true` iff
///   Jupiter returned a /v6/quote response whose `out_amount * (1 - slippage)`
///   covers `total_debt + flash_fee`. `false` triggers iterative.
pub fn decide_unwind_strategy(
    obligation: &DecodedObligation,
    sol_reserve_pubkey: &Pubkey,
    jitosol_reserve_pubkey: &Pubkey,
    sol_reserve_flash_cap_lamports: u64,
    jupiter_quote_available_and_in_slippage: bool,
) -> UnwindStrategy {
    let total_debt_sol_lamports: u64 = obligation
        .borrows
        .iter()
        .filter(|b| &b.reserve == sol_reserve_pubkey)
        .map(|b| (b.borrowed_amount_sf >> 60) as u64)
        .sum();
    let total_collateral_jitosol_ctokens: u64 = obligation
        .deposits
        .iter()
        .filter(|d| &d.reserve == jitosol_reserve_pubkey)
        .map(|d| d.deposited_amount)
        .sum();

    if total_debt_sol_lamports == 0 && total_collateral_jitosol_ctokens == 0 {
        return UnwindStrategy::Noop;
    }

    // FlashLoan path requires BOTH a flash cap covering the debt AND a
    // valid Jupiter quote.
    if total_debt_sol_lamports > 0
        && sol_reserve_flash_cap_lamports >= total_debt_sol_lamports
        && jupiter_quote_available_and_in_slippage
    {
        // Safety buffer: next accrued-interest tick + flash fee. Bundle
        // builder computes the exact fee from the reserve config; this
        // strategy only signals which path to take. We pass the headline
        // debt amount here; the bundle builder adds the flash fee in
        // [`build_unwind_flash_bundle`]'s `flash_repay_amount` arg.
        return UnwindStrategy::FlashLoan {
            flash_amount_lamports: total_debt_sol_lamports,
        };
    }

    // Fallback: iterative. Size rounds = MAX_LEVERAGE_LOOP_ROUNDS, the same
    // bound lever-up uses. The caller's per-round δ math sizes the actual
    // withdraw amount each round.
    UnwindStrategy::Iterative {
        rounds: caps::MAX_LEVERAGE_LOOP_ROUNDS,
    }
}

/// Compute the flash fee for `amount` SOL lamports against the SOL
/// reserve's `flash_loan_fee_sf` (fixed-point u128, divisor = 2^60).
///
/// Pure ceiling division: `ceil(amount * fee_sf / 2^60)`. Caller is
/// responsible for fetching `flash_loan_fee_sf` from the reserve account
/// (this builder cannot make RPC calls).
///
/// Returns 0 if `fee_sf == 0` (some reserves disable flash fees).
pub fn compute_flash_fee(amount_lamports: u64, flash_loan_fee_sf: u128) -> u64 {
    if flash_loan_fee_sf == 0 || amount_lamports == 0 {
        return 0;
    }
    let num = (amount_lamports as u128).saturating_mul(flash_loan_fee_sf);
    // ceil(num / 2^60)
    let scaled = num >> 60;
    let rem = num & ((1u128 << 60) - 1);
    let fee = if rem == 0 { scaled } else { scaled + 1 };
    fee.min(u64::MAX as u128) as u64
}

/// Build the single-tx flash-loan unwind bundle. Pure.
///
/// Bundle structure (per §3.2 of the unwind plan; CU + priority-fee ixns
/// are prepended by the caller's tx-build helper, NOT inside this fn —
/// hence "ix 4 = FlashBorrow" in the plan corresponds to bundle index 2
/// here because the compute-budget ixns are added at sign-and-send time):
///
/// ```text
///   0  ATA-create-idempotent(user, wSOL_MINT)
///   1  ATA-create-idempotent(user, jitoSOL_MINT)
///   2  FlashBorrowReserveLiquidity(sol_reserve, flash_amount_lamports)
///   3  RefreshReserve(jitoSOL)
///   4  RefreshReserve(SOL)
///   5  RefreshObligation(remaining=[jitoSOL, SOL])
///   6  RepayObligationLiquidityV2(sol_reserve, u64::MAX)
///   7  RefreshReserve(jitoSOL)
///   8  RefreshReserve(SOL)
///   9  RefreshObligation(remaining=[jitoSOL, SOL])
///  10  WithdrawObligationCollateralAndRedeemReserveCollateralV2(jitosol_reserve, u64::MAX)
///  11..K Jupiter swap ixns (supplied by caller via `jupiter_swap_ixns`)
///   K+1 FlashRepayReserveLiquidity(sol_reserve, flash_amount + flash_fee,
///                                   borrow_instruction_index = absolute tx index of FlashBorrow)
///   K+2 CloseAccount(wSOL ATA → user)
/// ```
///
/// `obligation_reserves`: the deposit-then-borrow reserve pubkeys for
/// `RefreshObligation`'s remaining_accounts. Must reflect the obligation's
/// current on-chain state (caller fetches once before calling).
///
/// `flash_borrow_absolute_tx_index`: the absolute tx-level index of the
/// FlashBorrow ixn (i.e. ix 2 here + the 2 compute-budget ixns prepended at
/// tx-build time = 4 for the standard build path). klend's FlashRepay
/// handler walks the instruction sysvar by absolute index, so this matters.
#[allow(clippy::too_many_arguments)]
pub fn build_unwind_flash_bundle(
    user: Pubkey,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
    flash_amount_lamports: u64,
    flash_fee_lamports: u64,
    jupiter_swap_ixns: Vec<Instruction>,
    obligation_reserves: &[Pubkey],
    flash_borrow_absolute_tx_index: u8,
) -> Result<Vec<Instruction>> {
    if flash_amount_lamports == 0 {
        return Err(anyhow!(
            "flash_amount_lamports must be > 0; use UnwindStrategy::Noop for empty positions"
        ));
    }
    // jitoSOL MINT sanity — the daemon's whole shape assumes the jitoSOL
    // reserve we pass in is the actual main-market jitoSOL reserve. Defence
    // in depth: assert the reserve's liquidity mint is the expected pubkey
    // so a misconfigured loader doesn't silently build a wrong-asset bundle.
    if jitosol_reserve.liquidity_mint != JITOSOL_MINT {
        return Err(anyhow!(
            "jitosol_reserve.liquidity_mint {} does not match JITOSOL_MINT {}",
            jitosol_reserve.liquidity_mint,
            JITOSOL_MINT
        ));
    }
    if sol_reserve.liquidity_mint != WSOL_MINT {
        return Err(anyhow!(
            "sol_reserve.liquidity_mint {} does not match WSOL_MINT {}",
            sol_reserve.liquidity_mint,
            WSOL_MINT
        ));
    }

    let mut ixs: Vec<Instruction> = Vec::with_capacity(16 + jupiter_swap_ixns.len());

    // [0,1] ATA-create-idempotent for wSOL + jitoSOL.
    ixs.push(create_associated_token_account_idempotent(
        &user,
        &user,
        &sol_reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));
    ixs.push(create_associated_token_account_idempotent(
        &user,
        &user,
        &jitosol_reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));

    // [2] FlashBorrow. The flash-borrow output ATA is the user's wSOL ATA
    // (already created by ix 0); klend's flash_borrow ix uses
    // `ata(user, reserve.liquidity_mint)` internally.
    ixs.push(flash_borrow_reserve_liquidity_ix(
        &user,
        sol_reserve,
        flash_amount_lamports,
    )?);

    // [3..5] pre-Repay refreshes.
    ixs.push(refresh_reserve_ix(jitosol_reserve));
    ixs.push(refresh_reserve_ix(sol_reserve));
    ixs.push(refresh_obligation_ix(
        &user,
        &sol_reserve.lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        obligation_reserves,
    ));

    // [6] RepayObligationLiquidityV2. `u64::MAX` = repay full debt slot;
    // klend clamps server-side to the current borrowed_amount_sf at tx-land
    // time, which means we don't have to worry about leaving sub-lamport
    // interest accrued between bundle-build and tx-land.
    ixs.push(repay_obligation_liquidity_v2_ix(
        &user,
        sol_reserve,
        u64::MAX,
        caps::MULTIPLY_OBLIGATION_SEED,
    )?);

    // [7..9] pre-Withdraw refreshes. RepayV2 marked the SOL reserve stale.
    ixs.push(refresh_reserve_ix(jitosol_reserve));
    ixs.push(refresh_reserve_ix(sol_reserve));
    ixs.push(refresh_obligation_ix(
        &user,
        &sol_reserve.lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        obligation_reserves,
    ));

    // [10] WithdrawAndRedeemV2. `u64::MAX` = withdraw the full deposited
    // cToken slot.
    ixs.push(
        withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
            &user,
            jitosol_reserve,
            u64::MAX,
            caps::MULTIPLY_OBLIGATION_SEED,
        )?,
    );

    // [11..K] Jupiter swap ixns. Caller is responsible for producing them
    // with input mint=jitoSOL, output mint=wSOL, amount=the redeemed
    // jitoSOL, slippage capped at caps::MAX_SLIPPAGE_BPS.
    ixs.extend(jupiter_swap_ixns);

    // [K+1] FlashRepay. amount = flash_amount + flash_fee.
    // borrow_instruction_index is the ABSOLUTE tx index of FlashBorrow.
    let flash_repay_amount = flash_amount_lamports
        .checked_add(flash_fee_lamports)
        .ok_or_else(|| anyhow!("flash_amount + flash_fee overflows u64"))?;
    ixs.push(flash_repay_reserve_liquidity_ix(
        &user,
        sol_reserve,
        flash_repay_amount,
        flash_borrow_absolute_tx_index,
    )?);

    // [K+2] Close wSOL ATA → unwraps leftover wSOL to native SOL.
    let user_wsol_ata = zerox1_defi_protocols::util::ata(&user, &WSOL_MINT);
    ixs.push(close_account(
        &TOKEN_PROGRAM_ID,
        &user_wsol_ata,
        &user,
        &user,
        &[],
    )?);

    Ok(ixs)
}

/// Build one round of the iterative fallback (no flash loan). Pure.
///
/// Per-round bundle:
/// ```text
///   0  RefreshReserve(jitoSOL)
///   1  RefreshReserve(SOL)
///   2  RefreshObligation(remaining = obligation_reserves)
///   3  WithdrawObligationCollateralAndRedeemReserveCollateralV2(jitoSOL, δ ctokens)
///      -- (jitoSOL liquidity now in user's jitoSOL ATA. Caller's tx adds
///         a jito_stake_pool::WithdrawSol or Jupiter swap ix here. We
///         leave that to the caller because the iterative-path swap leg
///         is materially different from the flash-loan-path swap leg —
///         iterative uses Jito direct redeem because it sizes δ to fit
///         the pool's instant-withdraw cap each round.)
///   4  RefreshReserve(jitoSOL)
///   5  RefreshReserve(SOL)
///   6  RefreshObligation(remaining = obligation_reserves)
///   7  RepayObligationLiquidityV2(SOL, δ SOL)
/// ```
///
/// The actual swap ixn is INSERTED by the caller between ix 3 and ix 4.
/// We don't bake it in here because the swap leg's shape (direct Jito
/// redeem vs Jupiter route) is decided per-round at runtime — and the
/// caller already has the swap ix list from its quote path.
///
/// `withdraw_jitosol_ctokens` is the cToken amount to redeem this round.
/// `repay_sol_lamports` is the SOL amount to repay this round (= the SOL
/// received from the swap, capped at the remaining debt).
pub fn build_unwind_iterative_round_bundle(
    user: Pubkey,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
    withdraw_jitosol_ctokens: u64,
    repay_sol_lamports: u64,
    obligation_reserves: &[Pubkey],
) -> Result<Vec<Instruction>> {
    if withdraw_jitosol_ctokens == 0 {
        return Err(anyhow!(
            "withdraw_jitosol_ctokens must be > 0 for an iterative round"
        ));
    }
    if repay_sol_lamports == 0 {
        return Err(anyhow!(
            "repay_sol_lamports must be > 0 for an iterative round"
        ));
    }
    if jitosol_reserve.liquidity_mint != JITOSOL_MINT {
        return Err(anyhow!(
            "jitosol_reserve.liquidity_mint {} does not match JITOSOL_MINT {}",
            jitosol_reserve.liquidity_mint,
            JITOSOL_MINT
        ));
    }
    if sol_reserve.liquidity_mint != WSOL_MINT {
        return Err(anyhow!(
            "sol_reserve.liquidity_mint {} does not match WSOL_MINT {}",
            sol_reserve.liquidity_mint,
            WSOL_MINT
        ));
    }

    let mut ixs: Vec<Instruction> = Vec::with_capacity(8);

    // [0..2] pre-Withdraw refreshes.
    ixs.push(refresh_reserve_ix(jitosol_reserve));
    ixs.push(refresh_reserve_ix(sol_reserve));
    ixs.push(refresh_obligation_ix(
        &user,
        &sol_reserve.lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        obligation_reserves,
    ));

    // [3] Withdraw collateral.
    ixs.push(
        withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
            &user,
            jitosol_reserve,
            withdraw_jitosol_ctokens,
            caps::MULTIPLY_OBLIGATION_SEED,
        )?,
    );

    // [4..6] pre-Repay refreshes (post-swap).
    ixs.push(refresh_reserve_ix(jitosol_reserve));
    ixs.push(refresh_reserve_ix(sol_reserve));
    ixs.push(refresh_obligation_ix(
        &user,
        &sol_reserve.lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        obligation_reserves,
    ));

    // [7] Repay debt.
    ixs.push(repay_obligation_liquidity_v2_ix(
        &user,
        sol_reserve,
        repay_sol_lamports,
        caps::MULTIPLY_OBLIGATION_SEED,
    )?);

    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;
    use zerox1_defi_protocols::constants::{KAMINO_FARMS_PROGRAM_ID, KAMINO_LEND_PROGRAM_ID};
    use zerox1_defi_protocols::protocols::kamino_loader::{ObligationBorrow, ObligationDeposit};
    use zerox1_defi_protocols::util::anchor_discriminator;

    fn dummy_reserve(lending_market: Pubkey, liquidity_mint: Pubkey) -> ReserveAccounts {
        ReserveAccounts {
            reserve: Pubkey::new_unique(),
            lending_market,
            lending_market_authority: Pubkey::new_unique(),
            liquidity_mint,
            liquidity_supply: Pubkey::new_unique(),
            collateral_mint: Pubkey::new_unique(),
            collateral_supply: Pubkey::new_unique(),
            fee_receiver: Pubkey::new_unique(),
            scope_prices: Pubkey::new_unique(),
            farm_collateral: Pubkey::default(),
            farm_debt: Pubkey::default(),
        }
    }

    fn empty_obligation(addr: Pubkey, lending_market: Pubkey, owner: Pubkey) -> DecodedObligation {
        DecodedObligation {
            address: addr,
            lending_market,
            owner,
            deposits: vec![],
            borrows: vec![],
            deposited_value_sf: 0,
            borrow_factor_adjusted_debt_value_sf: 0,
            borrowed_assets_market_value_sf: 0,
            allowed_borrow_value_sf: 0,
            unhealthy_borrow_value_sf: 0,
        }
    }

    // ── decide_unwind_strategy ─────────────────────────────────────────

    #[test]
    fn decide_strategy_empty_obligation_is_noop() {
        let obl = empty_obligation(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let s = decide_unwind_strategy(
            &obl,
            &Pubkey::new_unique(),
            &Pubkey::new_unique(),
            u64::MAX,
            true,
        );
        assert_eq!(s, UnwindStrategy::Noop);
    }

    #[test]
    fn decide_strategy_standard_position_picks_flashloan() {
        let lending_market = Pubkey::new_unique();
        let sol_reserve = Pubkey::new_unique();
        let jitosol_reserve = Pubkey::new_unique();
        let mut obl = empty_obligation(Pubkey::new_unique(), lending_market, Pubkey::new_unique());
        obl.deposits.push(ObligationDeposit {
            reserve: jitosol_reserve,
            deposited_amount: 1_000_000_000,
            market_value_sf: 0,
        });
        // 2 SOL of debt — sf-scaled: 2 * 1e9 lamports << 60.
        let debt_lamports: u64 = 2_000_000_000;
        obl.borrows.push(ObligationBorrow {
            reserve: sol_reserve,
            borrowed_amount_sf: (debt_lamports as u128) << 60,
            market_value_sf: 0,
            borrow_factor_adjusted_market_value_sf: 0,
        });

        // Flash cap covers debt; Jupiter quote OK → FlashLoan.
        let s = decide_unwind_strategy(
            &obl,
            &sol_reserve,
            &jitosol_reserve,
            debt_lamports * 10,
            true,
        );
        assert_eq!(
            s,
            UnwindStrategy::FlashLoan {
                flash_amount_lamports: debt_lamports
            }
        );
    }

    #[test]
    fn decide_strategy_falls_back_to_iterative_when_flash_cap_too_small() {
        let lending_market = Pubkey::new_unique();
        let sol_reserve = Pubkey::new_unique();
        let jitosol_reserve = Pubkey::new_unique();
        let mut obl = empty_obligation(Pubkey::new_unique(), lending_market, Pubkey::new_unique());
        obl.deposits.push(ObligationDeposit {
            reserve: jitosol_reserve,
            deposited_amount: 1_000_000_000,
            market_value_sf: 0,
        });
        let debt_lamports: u64 = 2_000_000_000;
        obl.borrows.push(ObligationBorrow {
            reserve: sol_reserve,
            borrowed_amount_sf: (debt_lamports as u128) << 60,
            market_value_sf: 0,
            borrow_factor_adjusted_market_value_sf: 0,
        });

        // Flash cap below debt → iterative.
        let s = decide_unwind_strategy(
            &obl,
            &sol_reserve,
            &jitosol_reserve,
            debt_lamports / 2,
            true,
        );
        assert_eq!(
            s,
            UnwindStrategy::Iterative {
                rounds: caps::MAX_LEVERAGE_LOOP_ROUNDS,
            }
        );
    }

    #[test]
    fn decide_strategy_falls_back_to_iterative_when_jupiter_quote_unavailable() {
        let lending_market = Pubkey::new_unique();
        let sol_reserve = Pubkey::new_unique();
        let jitosol_reserve = Pubkey::new_unique();
        let mut obl = empty_obligation(Pubkey::new_unique(), lending_market, Pubkey::new_unique());
        obl.deposits.push(ObligationDeposit {
            reserve: jitosol_reserve,
            deposited_amount: 1_000_000_000,
            market_value_sf: 0,
        });
        let debt_lamports: u64 = 2_000_000_000;
        obl.borrows.push(ObligationBorrow {
            reserve: sol_reserve,
            borrowed_amount_sf: (debt_lamports as u128) << 60,
            market_value_sf: 0,
            borrow_factor_adjusted_market_value_sf: 0,
        });

        let s = decide_unwind_strategy(
            &obl,
            &sol_reserve,
            &jitosol_reserve,
            debt_lamports * 10,
            false, // Jupiter quote not in slippage
        );
        assert_eq!(
            s,
            UnwindStrategy::Iterative {
                rounds: caps::MAX_LEVERAGE_LOOP_ROUNDS,
            }
        );
    }

    // ── compute_flash_fee ──────────────────────────────────────────────

    #[test]
    fn compute_flash_fee_zero_amount_is_zero() {
        assert_eq!(compute_flash_fee(0, 1 << 50), 0);
    }

    #[test]
    fn compute_flash_fee_zero_rate_is_zero() {
        assert_eq!(compute_flash_fee(1_000_000_000, 0), 0);
    }

    #[test]
    fn compute_flash_fee_30_bps_ceiling() {
        // 30 bps = 0.003 = 0.003 * 2^60 in sf.
        let fee_sf: u128 = ((1u128 << 60) * 3) / 1000;
        let amount: u64 = 1_000_000_000; // 1 SOL
        let fee = compute_flash_fee(amount, fee_sf);
        // 30 bps of 1 SOL = 3_000_000 lamports (possibly +1 from ceiling).
        assert!(
            (3_000_000..=3_000_001).contains(&fee),
            "30 bps of 1 SOL should be ~3_000_000 lamports, got {}",
            fee
        );
    }

    // ── build_unwind_flash_bundle ──────────────────────────────────────

    fn make_setup() -> (
        Pubkey,
        Pubkey,
        ReserveAccounts,
        ReserveAccounts,
        Vec<Pubkey>,
    ) {
        let user = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let sol_reserve = dummy_reserve(lending_market, WSOL_MINT);
        let jitosol_reserve = dummy_reserve(lending_market, JITOSOL_MINT);
        let obligation_reserves = vec![jitosol_reserve.reserve, sol_reserve.reserve];
        (
            user,
            lending_market,
            sol_reserve,
            jitosol_reserve,
            obligation_reserves,
        )
    }

    fn jupiter_stub_ix() -> Instruction {
        // Stand-in: a single ixn targeting a non-klend, non-token program id
        // so the test can pin its position in the bundle without depending
        // on the Jupiter v6 program id constant being exported.
        Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![],
            data: vec![],
        }
    }

    #[test]
    fn flash_bundle_rejects_zero_flash_amount() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let err = build_unwind_flash_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            0,
            0,
            vec![jupiter_stub_ix()],
            &oblig_reserves,
            4,
        )
        .unwrap_err();
        assert!(err.to_string().contains("flash_amount_lamports"));
    }

    #[test]
    fn flash_bundle_rejects_wrong_mint_on_sol_reserve() {
        let (user, lending_market, _, jitosol_reserve, oblig_reserves) = make_setup();
        // SOL reserve constructed with WRONG mint — should reject.
        let wrong_sol = dummy_reserve(lending_market, Pubkey::new_unique());
        let err = build_unwind_flash_bundle(
            user,
            &wrong_sol,
            &jitosol_reserve,
            1_000_000_000,
            3_000_000,
            vec![jupiter_stub_ix()],
            &oblig_reserves,
            4,
        )
        .unwrap_err();
        assert!(err.to_string().contains("WSOL_MINT"));
    }

    #[test]
    fn flash_bundle_has_correct_ix_count_and_program_ids() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        // All three Jupiter stub ixns must share a program id so the
        // positional assertion below can pin "jupiter occupies [11..14]"
        // without depending on the (random) per-stub pubkey.
        let jupiter_program = Pubkey::new_unique();
        let stub = || Instruction {
            program_id: jupiter_program,
            accounts: vec![],
            data: vec![],
        };
        let jupiter_ixns = vec![stub(), stub(), stub()];
        let ixs = build_unwind_flash_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000_000,
            3_000_000,
            jupiter_ixns,
            &oblig_reserves,
            4,
        )
        .expect("build flash bundle");

        // 13 fixed-shape ixns + 3 Jupiter swap ixns = 16.
        // (2 ATA-create + 1 flash-borrow + 3 refreshes + 1 repay + 3 refreshes
        //  + 1 withdraw + 3 jupiter + 1 flash-repay + 1 close).
        assert_eq!(ixs.len(), 13 + 3);

        // First two ixns are ATA-create (ATA program id).
        let ata_program = zerox1_defi_protocols::constants::ASSOCIATED_TOKEN_PROGRAM_ID;
        assert_eq!(ixs[0].program_id, ata_program);
        assert_eq!(ixs[1].program_id, ata_program);
        // Ix 2 = FlashBorrow → klend.
        assert_eq!(ixs[2].program_id, KAMINO_LEND_PROGRAM_ID);
        // Ix 10 = WithdrawAndRedeemV2 → klend.
        assert_eq!(ixs[10].program_id, KAMINO_LEND_PROGRAM_ID);
        // Ix 11..14 = jupiter.
        for i in 11..14 {
            assert_eq!(ixs[i].program_id, jupiter_program);
        }
        // Ix 14 = FlashRepay → klend.
        assert_eq!(ixs[14].program_id, KAMINO_LEND_PROGRAM_ID);
        // Ix 15 = CloseAccount → spl-token.
        assert_eq!(ixs[15].program_id, TOKEN_PROGRAM_ID);
    }

    #[test]
    fn flash_bundle_repay_uses_max_sentinel_and_v2_discriminator() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let ixs = build_unwind_flash_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000_000,
            3_000_000,
            vec![jupiter_stub_ix()],
            &oblig_reserves,
            4,
        )
        .expect("build");
        let repay_disc = anchor_discriminator("global", "repay_obligation_liquidity_v2");
        // RepayV2 lives at index 6 by construction.
        let repay = &ixs[6];
        assert_eq!(repay.program_id, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(repay.data[..8], repay_disc);
        // Args = u64::MAX (8 bytes after disc).
        assert_eq!(
            u64::from_le_bytes(repay.data[8..16].try_into().unwrap()),
            u64::MAX
        );
        // Account count = 12 (v1 9 + v2 farm 3).
        assert_eq!(repay.accounts.len(), 12);
        // Farm-absent path: last 3 slots use KAMINO_LEND/KAMINO_FARMS sentinels.
        assert_eq!(repay.accounts[9].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(repay.accounts[10].pubkey, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(repay.accounts[11].pubkey, KAMINO_FARMS_PROGRAM_ID);
    }

    #[test]
    fn flash_bundle_withdraw_uses_max_sentinel_and_v2_discriminator() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let ixs = build_unwind_flash_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000_000,
            3_000_000,
            vec![jupiter_stub_ix()],
            &oblig_reserves,
            4,
        )
        .expect("build");
        let withdraw_disc = anchor_discriminator(
            "global",
            "withdraw_obligation_collateral_and_redeem_reserve_collateral_v2",
        );
        let withdraw = &ixs[10];
        assert_eq!(withdraw.program_id, KAMINO_LEND_PROGRAM_ID);
        assert_eq!(withdraw.data[..8], withdraw_disc);
        assert_eq!(
            u64::from_le_bytes(withdraw.data[8..16].try_into().unwrap()),
            u64::MAX
        );
        // Account count = 17 (v1 14 + v2 farm 3).
        assert_eq!(withdraw.accounts.len(), 17);
    }

    #[test]
    fn flash_bundle_flash_repay_amount_equals_borrow_plus_fee() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let flash_amount: u64 = 1_000_000_000;
        let flash_fee: u64 = 3_000_000;
        let ixs = build_unwind_flash_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            flash_amount,
            flash_fee,
            vec![jupiter_stub_ix()],
            &oblig_reserves,
            4,
        )
        .expect("build");

        let flash_repay_disc = anchor_discriminator("global", "flash_repay_reserve_liquidity");
        // FlashRepay is just before the closing CloseAccount.
        let flash_repay = ixs
            .iter()
            .find(|ix| {
                ix.program_id == KAMINO_LEND_PROGRAM_ID
                    && ix.data.len() >= 8
                    && ix.data[..8] == flash_repay_disc
            })
            .expect("FlashRepay present");
        // Args = u64 amount + u8 borrow_instruction_index = 17 bytes after disc.
        let repay_amount = u64::from_le_bytes(flash_repay.data[8..16].try_into().unwrap());
        let bix_index = flash_repay.data[16];
        assert_eq!(repay_amount, flash_amount + flash_fee);
        assert_eq!(bix_index, 4, "flash_borrow_instruction_index must be 4");
    }

    #[test]
    fn flash_bundle_refresh_obligation_precedes_repay_and_withdraw() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let ixs = build_unwind_flash_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000_000,
            3_000_000,
            vec![jupiter_stub_ix()],
            &oblig_reserves,
            4,
        )
        .expect("build");

        let refresh_obligation_disc = anchor_discriminator("global", "refresh_obligation");
        let repay_disc = anchor_discriminator("global", "repay_obligation_liquidity_v2");
        let withdraw_disc = anchor_discriminator(
            "global",
            "withdraw_obligation_collateral_and_redeem_reserve_collateral_v2",
        );

        let is_refresh_obl = |ix: &Instruction| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == refresh_obligation_disc
        };

        let repay_idx = ixs
            .iter()
            .position(|ix| ix.data.len() >= 8 && ix.data[..8] == repay_disc)
            .expect("repay present");
        let withdraw_idx = ixs
            .iter()
            .position(|ix| ix.data.len() >= 8 && ix.data[..8] == withdraw_disc)
            .expect("withdraw present");

        assert!(repay_idx > 0 && is_refresh_obl(&ixs[repay_idx - 1]));
        assert!(withdraw_idx > 0 && is_refresh_obl(&ixs[withdraw_idx - 1]));
        // And repay precedes withdraw.
        assert!(repay_idx < withdraw_idx);
    }

    #[test]
    fn flash_bundle_refresh_reserve_for_each_reserve_before_refresh_obligation() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let ixs = build_unwind_flash_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000_000,
            3_000_000,
            vec![jupiter_stub_ix()],
            &oblig_reserves,
            4,
        )
        .expect("build");

        let refresh_reserve_disc = anchor_discriminator("global", "refresh_reserve");
        let refresh_obligation_disc = anchor_discriminator("global", "refresh_obligation");

        let is_refresh_reserve_of = |ix: &Instruction, reserve: &Pubkey| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == refresh_reserve_disc
                && !ix.accounts.is_empty()
                && ix.accounts[0].pubkey == *reserve
        };

        let first_refresh_obl_idx = ixs
            .iter()
            .position(|ix| {
                ix.program_id == KAMINO_LEND_PROGRAM_ID
                    && ix.data.len() >= 8
                    && ix.data[..8] == refresh_obligation_disc
            })
            .expect("first RefreshObligation present");

        let jitosol_refreshed = ixs[..first_refresh_obl_idx]
            .iter()
            .any(|ix| is_refresh_reserve_of(ix, &jitosol_reserve.reserve));
        let sol_refreshed = ixs[..first_refresh_obl_idx]
            .iter()
            .any(|ix| is_refresh_reserve_of(ix, &sol_reserve.reserve));
        assert!(
            jitosol_refreshed,
            "RefreshReserve(jitoSOL) missing before first RefreshObligation"
        );
        assert!(
            sol_refreshed,
            "RefreshReserve(SOL) missing before first RefreshObligation"
        );
    }

    #[test]
    fn flash_bundle_close_wsol_is_last() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let ixs = build_unwind_flash_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000_000,
            3_000_000,
            vec![jupiter_stub_ix()],
            &oblig_reserves,
            4,
        )
        .expect("build");
        let last = ixs.last().expect("non-empty");
        assert_eq!(last.program_id, TOKEN_PROGRAM_ID);
        // CloseAccount discriminator on classic SPL Token is a single byte 9.
        assert_eq!(last.data[0], 9);
    }

    // ── build_unwind_iterative_round_bundle ────────────────────────────

    #[test]
    fn iterative_bundle_rejects_zero_withdraw() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let err = build_unwind_iterative_round_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            0,
            1_000_000,
            &oblig_reserves,
        )
        .unwrap_err();
        assert!(err.to_string().contains("withdraw_jitosol_ctokens"));
    }

    #[test]
    fn iterative_bundle_rejects_zero_repay() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let err = build_unwind_iterative_round_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000,
            0,
            &oblig_reserves,
        )
        .unwrap_err();
        assert!(err.to_string().contains("repay_sol_lamports"));
    }

    #[test]
    fn iterative_bundle_shape_withdraw_precedes_repay() {
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let ixs = build_unwind_iterative_round_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000,
            1_000_000,
            &oblig_reserves,
        )
        .expect("build iterative round");
        // 3 pre-refresh + withdraw + 3 pre-refresh + repay = 8 ixns
        assert_eq!(ixs.len(), 8);
        let withdraw_disc = anchor_discriminator(
            "global",
            "withdraw_obligation_collateral_and_redeem_reserve_collateral_v2",
        );
        let repay_disc = anchor_discriminator("global", "repay_obligation_liquidity_v2");
        let withdraw_idx = ixs
            .iter()
            .position(|ix| ix.data.len() >= 8 && ix.data[..8] == withdraw_disc)
            .expect("withdraw present");
        let repay_idx = ixs
            .iter()
            .position(|ix| ix.data.len() >= 8 && ix.data[..8] == repay_disc)
            .expect("repay present");
        assert!(
            withdraw_idx < repay_idx,
            "withdraw ({withdraw_idx}) must precede repay ({repay_idx})"
        );
    }
}
