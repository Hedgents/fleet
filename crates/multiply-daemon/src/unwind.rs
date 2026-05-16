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

use anyhow::{anyhow, Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::system_instruction;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use spl_token::instruction::{close_account, sync_native};
use tracing::{info, warn};
use zerox1_defi_protocols::{
    constants::{
        JITOSOL_MINT, KAMINO_MAIN_JITOSOL_RESERVE, KAMINO_MAIN_MARKET,
        KAMINO_MAIN_MARKET_LOOKUP_TABLE, KAMINO_MAIN_SOL_RESERVE, TOKEN_PROGRAM_ID, WSOL_MINT,
    },
    protocols::{
        jito::{self, StakePoolMeta},
        jito_loader::load_jito_pool,
        kamino::{
            derive_user_obligation_with_seed, flash_borrow_reserve_liquidity_ix,
            flash_repay_reserve_liquidity_ix, refresh_obligation_ix, refresh_reserve_ix,
            repay_obligation_liquidity_v2_ix,
            withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix, ReserveAccounts,
        },
        kamino_loader::{fetch_obligation, load_reserve, DecodedObligation},
    },
};
use zerox1_protocol::fleet::multiply::{ReportMultiplyWithdraw, WithdrawMultiply};
use zerox1_protocol::fleet::ReportHeader;

use crate::caps;
use crate::dispatch::DispatchCtx;

/// Compute budget per iterative-unwind round. One round contains:
///   - 2 compute-budget ixns (prepended by RpcContext)
///   - 6 refresh ixns (jitoSOL + SOL + obligation, twice)
///   - 1 WithdrawObligationCollateralAndRedeemReserveCollateralV2 (klend, v2 farm CPI)
///   - 1 jito WithdrawSol
///   - 3 SOL→wSOL wrap ixns (CreateATA-idempotent + system::transfer +
///     spl_token::sync_native, v0.3.2)
///   - 1 RepayObligationLiquidityV2 (klend, v2 farm CPI)
/// = ~13 ixns, ~28 distinct accounts. 1M CU is comfortable headroom;
/// matches the lever-up CU envelope. The 3 wrap ixns are sub-10k CU
/// combined (ATA-create-idempotent ~5k when account exists, system
/// transfer ~150, sync_native ~3k).
const UNWIND_ITER_CU_LIMIT: u32 = 1_000_000;

/// Priority fee in microlamports for unwind round txns.
const UNWIND_ITER_PRIORITY_FEE: u64 = 10_000;

/// Jito stake-pool withdrawal fee (10 bps) applied to the SOL output of
/// `withdraw_sol`. We size the per-round `repay_sol_lamports` as
/// `expected_sol × (10000 - JITO_WITHDRAW_FEE_BPS) / 10000` to leave a
/// safety margin so the repay never exceeds what landed in the user's
/// wallet. The pool's current fee is well under this; we conservatively
/// over-deduct rather than risk an `InsufficientFunds` on repay.
const JITO_WITHDRAW_FEE_BPS: u64 = 10;

/// Compute budget for the single-tx flash-loan unwind. Larger than a
/// lever-up round (which is 1M CU) because the unwind tx packs more ixns:
/// 2 compute-budget + 2 ATA-create + flash-borrow + 3 refreshes + repay +
/// 3 refreshes + withdraw + ~8 Jupiter swap + flash-repay + close = 22-25
/// ixns. The plan caps this at 1.4M CU.
#[allow(dead_code)]
pub const UNWIND_CU_LIMIT: u32 = 1_400_000;

/// Priority fee for the unwind tx, in microlamports. Mirrors lever-up.
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(clippy::too_many_arguments, dead_code)]
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
///   4  swap_ix (Jito stake-pool WithdrawSol — inline atomic redeem, OR
///      a sequence of Jupiter swap ixns; the caller chooses the leg.
///      Optional — if `swap_ixs` is empty the round contains only
///      withdraw + repay and the caller is responsible for ensuring the
///      user's wSOL ATA has enough SOL to repay).
///   5  CreateATA-idempotent(user, wSOL_MINT)     -- wrap leg --
///   6  system_program::transfer(user → user_wSOL_ATA, repay_sol_lamports)
///   7  spl_token::sync_native(user_wSOL_ATA)
///   8  RefreshReserve(jitoSOL)
///   9  RefreshReserve(SOL)
///   10 RefreshObligation(remaining = obligation_reserves)
///   11 RepayObligationLiquidityV2(SOL, δ SOL)             -- OR --
///      (no repay ix at all when `repay_sol_lamports == 0`, which is the
///      "final round drains collateral but obligation no longer has
///      borrowable debt" case — the wrap leg is also skipped in that case)
/// ```
///
/// v0.3.2: the wrap leg (ixns [5..7]) was added because `Jito::WithdrawSol`
/// delivers raw SOL (lamports) directly to the user's wallet, but
/// klend's `RepayObligationLiquidityV2` expects `user_source_liquidity`
/// to be the user's wSOL ATA (SOL the asset is wrapped as wSOL the SPL
/// token for repay purposes). Without these 3 ixns the repay fails with
/// `AccountNotInitialized (3012)` on `user_source_liquidity` because
/// the user's wSOL ATA never got materialised by the round bundle.
///
/// v0.3.1: the swap ixns now flow through this builder rather than being
/// spliced by the caller. The iterative path uses
/// `jito::withdraw_sol_ix` (jitoSOL → native SOL, no Jupiter dependency,
/// fits in one ix). Keeping the slice typed as `&[Instruction]` lets the
/// flash-loan path's future Jupiter integration share the same shape if
/// we ever swap the iterative leg for a Jupiter quote.
///
/// `withdraw_jitosol_ctokens` is the cToken amount to redeem this round.
/// `repay_sol_lamports` is the SOL amount to repay this round (= the SOL
/// received from the swap, conservatively haircut by the Jito withdrawal
/// fee). Pass `0` to skip the repay leg entirely — useful for the final
/// drain round when the prior rounds have already cleared the borrow.
pub fn build_unwind_iterative_round_bundle(
    user: Pubkey,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
    withdraw_jitosol_ctokens: u64,
    repay_sol_lamports: u64,
    obligation_reserves: &[Pubkey],
    swap_ixs: &[Instruction],
) -> Result<Vec<Instruction>> {
    if withdraw_jitosol_ctokens == 0 {
        return Err(anyhow!(
            "withdraw_jitosol_ctokens must be > 0 for an iterative round"
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

    let mut ixs: Vec<Instruction> = Vec::with_capacity(12 + swap_ixs.len());

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

    // [4..K] swap leg (Jito WithdrawSol in the v0.3.1 wiring; empty for
    // the final drain round).
    ixs.extend(swap_ixs.iter().cloned());

    // If we're not repaying this round (final-round drain after debt
    // already cleared), skip the wrap + post-swap refresh + repay block —
    // klend's RefreshObligation would still succeed but adds CU + size
    // for no behavioural benefit, and there's no debt to repay anyway.
    if repay_sol_lamports == 0 {
        return Ok(ixs);
    }

    // [K+1..K+3] SOL → wSOL wrap leg (v0.3.2).
    //
    // `Jito::WithdrawSol` (ix [4]) delivers native SOL straight into
    // `user`'s wallet. `RepayObligationLiquidityV2` (ix [K+7]) wants
    // `user_source_liquidity` to be the user's wSOL ATA. We bridge the
    // two by:
    //   1. ensuring the user's wSOL ATA exists (idempotent)
    //   2. transferring `repay_sol_lamports` of raw SOL into that ATA
    //   3. calling `sync_native` so SPL Token bumps the account's
    //      "amount" field to match the new lamport balance
    // After these three ixns the wSOL ATA holds exactly
    // `repay_sol_lamports` of wSOL (plus whatever balance was sitting in
    // it before, which is preserved). Without this bridge the repay
    // fails at sim with `AccountNotInitialized (3012)` on
    // `user_source_liquidity`.
    let user_wsol_ata = zerox1_defi_protocols::util::ata(&user, &WSOL_MINT);
    ixs.push(create_associated_token_account_idempotent(
        &user,
        &user,
        &WSOL_MINT,
        &TOKEN_PROGRAM_ID,
    ));
    ixs.push(system_instruction::transfer(
        &user,
        &user_wsol_ata,
        repay_sol_lamports,
    ));
    ixs.push(sync_native(&TOKEN_PROGRAM_ID, &user_wsol_ata)?);

    // [K+4..K+6] pre-Repay refreshes (post-swap).
    ixs.push(refresh_reserve_ix(jitosol_reserve));
    ixs.push(refresh_reserve_ix(sol_reserve));
    ixs.push(refresh_obligation_ix(
        &user,
        &sol_reserve.lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        obligation_reserves,
    ));

    // [K+7] Repay debt.
    ixs.push(repay_obligation_liquidity_v2_ix(
        &user,
        sol_reserve,
        repay_sol_lamports,
        caps::MULTIPLY_OBLIGATION_SEED,
    )?);

    Ok(ixs)
}

/// Error code returned in `ReportMultiplyWithdraw.header.error_code`
/// when the unwind cannot proceed because the **flash-loan path** still
/// requires Jupiter aggregator integration, OR because a per-round
/// simulation in the iterative path failed and we refuse to broadcast
/// blind.
///
/// v0.3.1: the **iterative** path is fully wired against the Jito
/// stake-pool's `WithdrawSol` ixn as its swap leg — Iterative no longer
/// returns this code on entry. The flash-loan path still bails here
/// because it requires Jupiter's swap-ixns adapter, which is independent
/// from the iterative wiring and lands in a follow-up. The same code
/// also surfaces if an iterative round's sim returns an error: we dump
/// `unwind_sim_log:` lines, refuse to broadcast that round, and return
/// this code with whatever `tx_signatures` already landed.
pub const ERR_JUPITER_INTEGRATION_PENDING: u32 = 11;

/// Error code returned when the obligation/reserve fetch fails. Maps
/// to error_code = 12 from §4.3 of the unwind plan. Unused in v0.3.0
/// (we propagate the underlying `anyhow!` from the loader instead);
/// kept here so the per-phase code is reserved for v0.3.1.
#[allow(dead_code)]
pub const ERR_RESERVE_LOADER_FAILED: u32 = 12;

/// Either simulate the unwind or actually submit it (per
/// `ctx.simulate_only`). Mirrors [`crate::leverage::run_or_simulate`].
///
/// v0.3.1 wiring:
/// * Loads obligation + reserves + SOL flash cap via RPC.
/// * Decides strategy via [`decide_unwind_strategy`].
/// * `Noop` → returns ok=true with empty `tx_signatures` and the
///   wallet's current SOL balance.
/// * `Iterative` → runs up to `caps::MAX_LEVERAGE_LOOP_ROUNDS` rounds of
///   withdraw-collateral + Jito `WithdrawSol` + repay against the
///   obligation, sizing each round's δ off the live obligation state.
///   `simulate_only=true` stops after one sim-validated round (chain
///   unchanged).
/// * `FlashLoan` → still returns `ERR_JUPITER_INTEGRATION_PENDING`. The
///   flash path requires Jupiter swap ixns to land alongside the
///   FlashRepay; that adapter is independent from the iterative wiring
///   and follows in a subsequent release.
///
/// `simulate_only`: only the iterative path branches on this. The
/// flash-loan path remains unbroadcast end-to-end in v0.3.1.
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    withdraw: &WithdrawMultiply,
    conv: [u8; 16],
) -> Result<ReportMultiplyWithdraw> {
    let user = ctx.wallet.pubkey();
    let lending_market = KAMINO_MAIN_MARKET;

    // Deadline gate (post-Approve; the Assign-side gate is in dispatch).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if withdraw.deadline_unix > 0 && withdraw.deadline_unix < now {
        return Ok(ReportMultiplyWithdraw {
            header: ReportHeader::err(conv, 3),
            final_usdc_lamports: 0,
            residual_sol_lamports: 0,
            tx_signatures: vec![],
        });
    }

    info!(
        simulate_only = ctx.simulate_only,
        max_slippage_bps = withdraw.max_slippage_bps,
        "unwind starting"
    );

    // Load obligation + reserves.
    let sol_reserve = load_reserve(
        &ctx.rpc.client,
        &KAMINO_MAIN_SOL_RESERVE,
        WSOL_MINT,
        &lending_market,
    )
    .await
    .with_context(|| "load SOL reserve")?;
    let jitosol_reserve = load_reserve(
        &ctx.rpc.client,
        &KAMINO_MAIN_JITOSOL_RESERVE,
        JITOSOL_MINT,
        &lending_market,
    )
    .await
    .with_context(|| "load jitoSOL reserve")?;

    let obligation_addr = derive_user_obligation_with_seed(
        &user,
        &lending_market,
        caps::MULTIPLY_OBLIGATION_SEED.0,
        caps::MULTIPLY_OBLIGATION_SEED.1,
    );
    let decoded_opt = fetch_obligation(&ctx.rpc.client, &obligation_addr)
        .await
        .with_context(|| "fetch obligation")?;

    // No obligation → nothing to unwind.
    let Some(obligation) = decoded_opt else {
        info!(?conv, "obligation does not exist; nothing to unwind (Noop)");
        let residual_sol = ctx
            .rpc
            .client
            .get_balance(&user)
            .await
            .with_context(|| "fetch wallet SOL balance")?;
        return Ok(ReportMultiplyWithdraw {
            header: ReportHeader::ok(conv),
            final_usdc_lamports: 0,
            residual_sol_lamports: residual_sol,
            tx_signatures: vec![],
        });
    };

    // SOL flash cap. The flash-loan handler reads the reserve's
    // `liquidity.available_amount`; we use the same field.
    let sol_reserve_liq = zerox1_defi_protocols::protocols::kamino_loader::fetch_reserve_liquidity(
        &ctx.rpc.client,
        &KAMINO_MAIN_SOL_RESERVE,
    )
    .await
    .with_context(|| "fetch SOL reserve liquidity")?;

    let strategy = decide_unwind_strategy(
        &obligation,
        &sol_reserve.reserve,
        &jitosol_reserve.reserve,
        sol_reserve_liq.available_amount,
        // v0.3.1: the iterative path now broadcasts via Jito direct
        // redeem and needs no Jupiter quote. The flash-loan path still
        // needs Jupiter ixns alongside FlashRepay (different ix-graph
        // shape, can't reuse Jito redeem because Jito's WithdrawSol
        // delivers native SOL but the flash loan repays wSOL — the
        // iterative path repays out of the wallet's wSOL ATA after the
        // jitoSOL ATA has dripped into native SOL, but flash-loan
        // requires the wSOL to materialize INSIDE the same tx). So we
        // still set this `false` to short-circuit to Iterative; the
        // FlashLoan path stays gated on the pending Jupiter adapter.
        false,
    );

    info!(?strategy, "unwind strategy decided");

    let residual_sol = ctx
        .rpc
        .client
        .get_balance(&user)
        .await
        .with_context(|| "fetch wallet SOL balance")?;

    match strategy {
        UnwindStrategy::Noop => {
            info!(?conv, "Noop unwind — obligation empty");
            Ok(ReportMultiplyWithdraw {
                header: ReportHeader::ok(conv),
                final_usdc_lamports: 0,
                residual_sol_lamports: residual_sol,
                tx_signatures: vec![],
            })
        }
        UnwindStrategy::FlashLoan {
            flash_amount_lamports,
        } => {
            warn!(
                ?conv,
                flash_amount_lamports,
                "FlashLoan strategy selected but Jupiter integration not yet wired — \
                 returning ERR_JUPITER_INTEGRATION_PENDING. Position state is preserved; \
                 the iterative path covers this case via the `false` jupiter_quote_available \
                 short-circuit, so this arm should be unreachable through v0.3.1."
            );
            Ok(ReportMultiplyWithdraw {
                header: ReportHeader::err(conv, ERR_JUPITER_INTEGRATION_PENDING),
                final_usdc_lamports: 0,
                residual_sol_lamports: residual_sol,
                tx_signatures: vec![],
            })
        }
        UnwindStrategy::Iterative { rounds } => {
            info!(
                ?conv,
                rounds, "Iterative strategy selected — running unwind rounds"
            );
            run_iterative_unwind(ctx, conv, rounds, &sol_reserve, &jitosol_reserve).await
        }
    }
}

/// Per-round iterative unwind. Each round:
///   1. Re-fetches the obligation (drives δ-sizing off live state).
///   2. Computes δ_jitosol = ceil(remaining_jitosol_ctokens / rounds_left).
///   3. Computes expected_sol = pool.jitosol_to_sol(δ_jitosol).
///   4. Computes repay_sol = expected_sol × (10000 - 10) / 10000   (Jito fee haircut),
///      then min(repay_sol, remaining_debt_lamports).
///   5. Builds the round bundle (withdraw + jito_withdraw + repay).
///   6. Sims; if sim ok, broadcasts (unless `simulate_only`); otherwise
///      returns ERR_JUPITER_INTEGRATION_PENDING with sim logs dumped.
///   7. Re-queries obligation; if no debt + no collateral → finished.
///
/// The last round (debt already cleared but collateral remains) passes
/// `repay_sol_lamports = 0` to the bundle builder, which skips the repay
/// block entirely. This keeps the bundle to a withdraw + swap shape that
/// klend accepts even when borrowed_amount_sf is zero.
async fn run_iterative_unwind(
    ctx: &DispatchCtx,
    conv: [u8; 16],
    max_rounds: u8,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
) -> Result<ReportMultiplyWithdraw> {
    let user = ctx.wallet.pubkey();
    let lending_market = KAMINO_MAIN_MARKET;
    let alts = [KAMINO_MAIN_MARKET_LOOKUP_TABLE];

    let obligation_addr = derive_user_obligation_with_seed(
        &user,
        &lending_market,
        caps::MULTIPLY_OBLIGATION_SEED.0,
        caps::MULTIPLY_OBLIGATION_SEED.1,
    );

    let mut tx_signatures: Vec<String> = Vec::with_capacity(max_rounds as usize);

    for round in 1..=max_rounds {
        // Re-fetch obligation per round.
        let Some(oblig) = fetch_obligation(&ctx.rpc.client, &obligation_addr)
            .await
            .with_context(|| format!("round {round}: re-fetch obligation"))?
        else {
            info!(?conv, round, "obligation closed mid-unwind; finished early");
            break;
        };

        let remaining_jitosol_ctokens: u64 = oblig
            .deposits
            .iter()
            .filter(|d| d.reserve == jitosol_reserve.reserve)
            .map(|d| d.deposited_amount)
            .sum();
        let remaining_debt_sol_lamports: u64 = oblig
            .borrows
            .iter()
            .filter(|b| b.reserve == sol_reserve.reserve)
            .map(|b| (b.borrowed_amount_sf >> 60) as u64)
            .sum();

        if remaining_jitosol_ctokens == 0 && remaining_debt_sol_lamports == 0 {
            info!(?conv, round, "obligation empty; unwind complete");
            break;
        }
        if remaining_jitosol_ctokens == 0 {
            warn!(
                ?conv,
                round,
                remaining_debt_sol_lamports,
                "obligation has SOL debt but no jitoSOL collateral — cannot unwind via Jito redeem"
            );
            return Ok(ReportMultiplyWithdraw {
                header: ReportHeader::err(conv, ERR_JUPITER_INTEGRATION_PENDING),
                final_usdc_lamports: 0,
                residual_sol_lamports: ctx.rpc.client.get_balance(&user).await.unwrap_or_default(),
                tx_signatures,
            });
        }

        // δ-sizing: divide remaining collateral evenly across remaining
        // rounds. Last round drains everything. Use ceiling division so
        // we never under-withdraw and leave dust.
        let rounds_left = (max_rounds - round + 1) as u64;
        let delta_jitosol_ctokens: u64 = if round == max_rounds {
            remaining_jitosol_ctokens
        } else {
            (remaining_jitosol_ctokens + rounds_left - 1) / rounds_left
        };

        // Estimate SOL output. The Jito pool is loaded once per round —
        // its exchange rate moves slowly (epoch boundary) so per-round
        // loads guarantee fresh rates without adding meaningful latency.
        let jito_pool: StakePoolMeta = load_jito_pool(&ctx.rpc.client)
            .await
            .with_context(|| format!("round {round}: load Jito stake pool"))?;
        // Note: the jitoSOL the obligation holds is denominated in
        // *collateral cTokens* on the Kamino reserve, not raw jitoSOL.
        // For Kamino's jitoSOL reserve the cToken:underlying ratio is
        // pegged 1:1 at protocol level until liquidation events shift
        // it; in production this has held since reserve genesis. We
        // therefore treat δ_jitosol_ctokens ≈ δ_jitosol_underlying for
        // the purpose of estimating the swap output. The bundle's
        // WithdrawObligationCollateralAndRedeemReserveCollateralV2 ix
        // performs the cToken→underlying redeem at the actual on-chain
        // rate; if it diverges meaningfully the user's jitoSOL ATA will
        // hold less than δ_jitosol_ctokens and the subsequent Jito
        // WithdrawSol will fail with `InsufficientFunds` rather than
        // silently transact at a wrong rate.
        let expected_sol_lamports = jito_pool.jitosol_to_sol_lamports(delta_jitosol_ctokens);
        let repay_after_fee =
            expected_sol_lamports.saturating_mul(10_000 - JITO_WITHDRAW_FEE_BPS) / 10_000;
        let repay_sol_lamports = repay_after_fee.min(remaining_debt_sol_lamports);

        info!(
            ?conv,
            round,
            remaining_jitosol_ctokens,
            remaining_debt_sol_lamports,
            delta_jitosol_ctokens,
            expected_sol_lamports,
            repay_sol_lamports,
            "round sizing"
        );

        // Build swap ix.
        let swap_ix = jito::withdraw_sol_ix(&user, &jito_pool, delta_jitosol_ctokens)
            .with_context(|| format!("round {round}: build jito WithdrawSol ix"))?;

        // Build the obligation_reserves slice in the same order klend
        // expects on the obligation account: deposits first, then borrows.
        let obligation_reserves: Vec<Pubkey> = {
            let mut v: Vec<Pubkey> = Vec::new();
            for d in &oblig.deposits {
                if !v.contains(&d.reserve) {
                    v.push(d.reserve);
                }
            }
            for b in &oblig.borrows {
                if !v.contains(&b.reserve) {
                    v.push(b.reserve);
                }
            }
            v
        };

        let ixs = build_unwind_iterative_round_bundle(
            user,
            sol_reserve,
            jitosol_reserve,
            delta_jitosol_ctokens,
            repay_sol_lamports,
            &obligation_reserves,
            std::slice::from_ref(&swap_ix),
        )
        .with_context(|| format!("round {round}: build iterative round bundle"))?;

        // Audit-fix I1: structural authority boundary. Same shape as
        // leverage.rs — verify all ixns target whitelisted programs.
        ctx.whitelist
            .verify_ixns(&ixs)
            .context("whitelist check on iterative unwind round ixns")?;

        // Always sim first.
        let sim = ctx
            .rpc
            .build_sign_simulate_with_alts(
                ixs.clone(),
                ctx.wallet.keypair(),
                UNWIND_ITER_CU_LIMIT,
                UNWIND_ITER_PRIORITY_FEE,
                &alts,
            )
            .await
            .with_context(|| format!("round {round}: simulate iterative unwind tx"))?;

        let (layout_valid, summary) = zerox1_defi_runtime::rpc::classify_simulation(&sim);
        if let Some(logs) = sim.logs.as_ref() {
            let log_level_warn = sim.err.is_some();
            for (i, line) in logs.iter().enumerate() {
                if log_level_warn {
                    warn!(round, unwind_sim_log_idx = i, "unwind_sim_log: {}", line);
                } else {
                    info!(round, unwind_sim_log_idx = i, "unwind_sim_log: {}", line);
                }
            }
        }

        if sim.err.is_some() {
            warn!(
                ?conv,
                round,
                layout_valid,
                summary = %summary,
                err = ?sim.err,
                "round sim FAILED — stopping iterative unwind"
            );
            return Ok(ReportMultiplyWithdraw {
                header: ReportHeader::err(conv, ERR_JUPITER_INTEGRATION_PENDING),
                final_usdc_lamports: 0,
                residual_sol_lamports: ctx.rpc.client.get_balance(&user).await.unwrap_or_default(),
                tx_signatures,
            });
        }
        info!(
            ?conv,
            round,
            layout_valid,
            summary = %summary,
            ix_count = ixs.len(),
            "round sim ok"
        );

        if ctx.simulate_only {
            // sim-only mode: prove the bundle shape is valid, then stop
            // (chain state unchanged, re-fetching would yield the same
            // obligation forever).
            info!(
                ?conv,
                round, "simulate-only: stopping after one sim'd round (chain state unchanged)"
            );
            break;
        }

        // Broadcast.
        let sig = ctx
            .rpc
            .build_sign_send_with_alts(
                ixs,
                ctx.wallet.keypair(),
                UNWIND_ITER_CU_LIMIT,
                UNWIND_ITER_PRIORITY_FEE,
                &alts,
            )
            .await
            .with_context(|| format!("round {round}: broadcast iterative unwind tx"))?;
        info!(?conv, round, sig = %sig, "round committed");
        tx_signatures.push(sig.to_string());
    }

    // Final wallet balance.
    let residual_sol = ctx
        .rpc
        .client
        .get_balance(&user)
        .await
        .with_context(|| "fetch wallet SOL balance after unwind")?;

    Ok(ReportMultiplyWithdraw {
        header: ReportHeader::ok(conv),
        // v0.3.1 unwinds back to SOL only — final wSOL/SOL stays in the
        // wallet. The USDC sweep leg (Jupiter SOL→USDC) lands alongside
        // emergency-destination redirection in a follow-up commit.
        final_usdc_lamports: 0,
        residual_sol_lamports: residual_sol,
        tx_signatures,
    })
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
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("withdraw_jitosol_ctokens"));
    }

    fn dummy_swap_ix() -> Instruction {
        // Stand-in Jito WithdrawSol — uses SPL_STAKE_POOL_PROGRAM_ID so
        // the bundle's whitelist check (in production) would let it
        // through. Test only cares about positional shape.
        Instruction {
            program_id: zerox1_defi_protocols::constants::SPL_STAKE_POOL_PROGRAM_ID,
            accounts: vec![],
            data: vec![0x10],
        }
    }

    #[test]
    fn iterative_bundle_shape_with_swap_leg() {
        // v0.3.2: bundle now inlines the Jito WithdrawSol AND the
        // SOL→wSOL wrap leg (CreateATA + system::transfer + sync_native)
        // between withdraw_collateral and the post-swap refresh+repay
        // block. Expected ix order:
        //   refresh(jito) refresh(sol) refresh_obl withdraw_collateral
        //   jito_withdraw_sol
        //   create_wsol_ata system_transfer sync_native
        //   refresh(jito) refresh(sol) refresh_obl repay
        // = 12 ixs.
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let swap = dummy_swap_ix();
        let ixs = build_unwind_iterative_round_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000,
            1_000_000,
            &oblig_reserves,
            std::slice::from_ref(&swap),
        )
        .expect("build iterative round");
        assert_eq!(
            ixs.len(),
            12,
            "3 refresh + withdraw + swap + 3 wrap + 3 refresh + repay"
        );
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
        // Swap leg lives immediately after withdraw_collateral.
        assert_eq!(
            ixs[withdraw_idx + 1].program_id,
            zerox1_defi_protocols::constants::SPL_STAKE_POOL_PROGRAM_ID,
            "swap ix should be at withdraw_idx+1"
        );
        assert_eq!(ixs[withdraw_idx + 1].data[0], 0x10);

        // Wrap leg sits between swap and the post-swap refresh block.
        // [withdraw_idx+2] CreateATA-idempotent → ATA program
        // [withdraw_idx+3] system_program::transfer → System program
        // [withdraw_idx+4] sync_native → SPL Token program
        let ata_program = zerox1_defi_protocols::constants::ASSOCIATED_TOKEN_PROGRAM_ID;
        let system_program = zerox1_defi_protocols::constants::SYSTEM_PROGRAM_ID;
        assert_eq!(
            ixs[withdraw_idx + 2].program_id,
            ata_program,
            "CreateATA-idempotent for wSOL must follow swap leg"
        );
        assert_eq!(
            ixs[withdraw_idx + 3].program_id,
            system_program,
            "system::transfer must follow CreateATA"
        );
        assert_eq!(
            ixs[withdraw_idx + 4].program_id,
            TOKEN_PROGRAM_ID,
            "sync_native must follow system::transfer"
        );
        // sync_native discriminator is single byte 17 on classic SPL Token.
        assert_eq!(ixs[withdraw_idx + 4].data[0], 17);
        // wrap must precede the repay.
        assert!(
            withdraw_idx + 4 < repay_idx,
            "sync_native (idx {}) must precede repay (idx {repay_idx})",
            withdraw_idx + 4
        );
    }

    #[test]
    fn iterative_bundle_drain_round_skips_repay_when_repay_amount_zero() {
        // Final drain round: debt already cleared in prior rounds. Bundle
        // must omit the post-swap refresh+repay block — 5 ixs total.
        let (user, _, sol_reserve, jitosol_reserve, oblig_reserves) = make_setup();
        let swap = dummy_swap_ix();
        let ixs = build_unwind_iterative_round_bundle(
            user,
            &sol_reserve,
            &jitosol_reserve,
            1_000_000,
            0, // drain — skip repay
            &oblig_reserves,
            std::slice::from_ref(&swap),
        )
        .expect("build drain round");
        assert_eq!(ixs.len(), 5, "3 refresh + withdraw + swap, no repay block");
        let repay_disc = anchor_discriminator("global", "repay_obligation_liquidity_v2");
        assert!(
            !ixs.iter()
                .any(|ix| ix.data.len() >= 8 && ix.data[..8] == repay_disc),
            "drain-round bundle must not contain a Repay ix"
        );
    }

    #[test]
    fn iterative_bundle_rejects_wrong_mint() {
        let (user, lending_market, _, jitosol_reserve, oblig_reserves) = make_setup();
        let wrong_sol = dummy_reserve(lending_market, Pubkey::new_unique());
        let err = build_unwind_iterative_round_bundle(
            user,
            &wrong_sol,
            &jitosol_reserve,
            1_000_000,
            1_000_000,
            &oblig_reserves,
            &[],
        )
        .unwrap_err();
        assert!(err.to_string().contains("WSOL_MINT"));
    }
}
