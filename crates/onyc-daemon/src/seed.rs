//! ONyc seed flow: USDC → ONyc → Kamino obligation collateral.
//!
//! Replaces multiply's USDC → SOL → jitoSOL → Kamino pipeline. Much
//! shorter because there's no staking step — ONyc is a direct SPL token
//! we acquire via Orca (routed by Jupiter aggregator).
//!
//! Two entry points:
//!   * [`seed_with_usdc`] — orchestrator-driven path. The orchestrator
//!     routes `usdc_lamports` USDC into the daemon's wallet, the daemon
//!     swaps USDC→ONyc via Jupiter, then this module deposits the ONyc
//!     as obligation collateral.
//!   * [`maybe_seed_obligation`] — operator-trigger path. Reads the
//!     wallet's idle ONyc balance and deposits it as collateral if the
//!     obligation has no collateral yet.
//!
//! Both paths produce the same on-chain shape: a Kamino isolated-market
//! obligation under [`caps::ONYC_OBLIGATION_SEED`] with ONyc collateral,
//! ready for the leverage loop to borrow USDC against.

use anyhow::{Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use tracing::{debug, info, warn};
use zerox1_defi_protocols::{
    constants::{KAMINO_ONYC_MARKET, KAMINO_ONYC_RESERVE, ONYC_MINT, TOKEN_PROGRAM_ID},
    protocols::{
        jupiter::{build_usdc_to_onyc_swap_tx, JupiterSwap},
        kamino::{
            deposit_reserve_liquidity_and_obligation_collateral_v2_ix,
            derive_user_obligation_with_seed, init_user_metadata_ix, initialize_obligation_ix,
            refresh_obligation_ix, refresh_reserve_ix, ReserveAccounts,
        },
        kamino_loader::{fetch_obligation, load_reserve, user_metadata_exists},
    },
};

use crate::caps;
use crate::dispatch::DispatchCtx;

/// Compute budget for the seed deposit bundle. ONyc seed is a single
/// init_user_metadata? + init_obligation? + ATA + RefreshReserve +
/// RefreshObligation + DepositCollateralV2 — roughly half the CU budget
/// of multiply's seed (which also had Jito DepositSol).
const SEED_CU_LIMIT: u32 = 600_000;
const SEED_PRIORITY_FEE: u64 = 10_000;

/// Minimum ONyc lamports (9-decimal) below which seeding is skipped —
/// would round to dust and waste a transaction. 0.001 ONyc ≈ $0.001 at
/// NAV ~$1.11. Cap is conservative to avoid burning fees on stale dust.
pub const SEED_MIN_ONYC_LAMPORTS: u64 = 1_000_000;

/// Pure decision: how much wallet ONyc to deposit as collateral.
#[derive(Debug, PartialEq, Eq)]
pub enum SeedDecision {
    /// Deposit this many ONyc lamports as collateral.
    Deposit(u64),
    /// Wallet ONyc balance is below the dust threshold.
    InsufficientOnycBalance { onyc_lamports: u64 },
}

/// Pure helper: clamp the ONyc deposit to the operator's max-position cap.
/// `max_position_usdc_lamports` is interpreted as a *USD-equivalent* cap
/// from the operator CLI; we use the rough heuristic that 1 ONyc ≈ $1.11
/// (current NAV), so the lamport equivalent is `cap_usdc_lamports × 1000 /
/// 1110`. Conservative under-estimates of NAV are safer than over-estimates
/// here (under-cap, not over-cap).
fn cap_onyc_to_usdc_equivalent(onyc_lamports: u64, cap_usdc_lamports: u64) -> u64 {
    // USDC is 6 dp; ONyc is 9 dp. usdc_lamports × 10^3 → onyc lamport scale.
    // Then divide by NAV in cents (~111 cents per ONyc).
    let cap_onyc = cap_usdc_lamports.saturating_mul(1_000) / 111 * 100;
    onyc_lamports.min(cap_onyc)
}

/// Decide how much of the wallet's ONyc to deposit.
pub fn decide_seed_amount(wallet_onyc_lamports: u64, max_position_usdc_lamports: u64) -> SeedDecision {
    if wallet_onyc_lamports < SEED_MIN_ONYC_LAMPORTS {
        return SeedDecision::InsufficientOnycBalance {
            onyc_lamports: wallet_onyc_lamports,
        };
    }
    SeedDecision::Deposit(cap_onyc_to_usdc_equivalent(
        wallet_onyc_lamports,
        max_position_usdc_lamports,
    ))
}

/// Build the ONyc-deposit instruction bundle.
///
/// Mirrors the structure of multiply's `build_seed_bundle` but without
/// the Jito DepositSol step (ONyc is already in the wallet at this point
/// — either swapped by `seed_with_usdc` or pre-funded by the operator).
///
/// `obligation_reserve_accounts` is every reserve currently referenced
/// by the obligation. klend's RefreshObligation requires each to have
/// been refreshed in-tx — see multiply's rc52 fix. For a fresh wallet
/// this slice is empty.
pub fn build_seed_bundle(
    user: &Pubkey,
    onyc_reserve: &ReserveAccounts,
    deposit_onyc_lamports: u64,
    user_metadata_missing: bool,
    obligation_already_exists: bool,
    obligation_reserve_accounts: &[&ReserveAccounts],
) -> Result<Vec<Instruction>> {
    let mut ixs: Vec<Instruction> = Vec::new();

    if user_metadata_missing {
        info!(%user, "user_metadata not found — prepending init_user_metadata_ix");
        ixs.push(init_user_metadata_ix(user));
    }

    // Kamino v2 seed bundle:
    //   * InitializeObligation (skip when PDA exists)
    //   * ATA-create-idempotent for the ONyc liquidity ATA
    //   * RefreshReserve for every reserve already in the obligation +
    //     the ONyc reserve we're about to deposit into
    //   * RefreshObligation
    //   * DepositReserveLiquidityAndObligationCollateralV2(ONyc)

    if !obligation_already_exists {
        ixs.push(
            initialize_obligation_ix(
                user,
                &onyc_reserve.lending_market,
                caps::ONYC_OBLIGATION_SEED,
            )
            .context("build initialize_obligation_ix for ONyc market")?,
        );
    } else {
        debug!("obligation already exists; skipping InitializeObligation in seed bundle");
    }

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &onyc_reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));

    // Refresh every reserve already in the obligation; klend trips
    // InvalidAccountInput (0x1776) if any remaining_account in
    // refresh_obligation_ix isn't fresh in-tx.
    for r in obligation_reserve_accounts {
        ixs.push(refresh_reserve_ix(r));
    }
    // Always refresh the ONyc reserve we're depositing into (dedupe
    // against the obligation_reserve_accounts loop above).
    if !obligation_reserve_accounts
        .iter()
        .any(|r| r.reserve == onyc_reserve.reserve)
    {
        ixs.push(refresh_reserve_ix(onyc_reserve));
    }
    let obligation_reserves: Vec<Pubkey> = obligation_reserve_accounts
        .iter()
        .map(|r| r.reserve)
        .collect();
    ixs.push(refresh_obligation_ix(
        user,
        &onyc_reserve.lending_market,
        caps::ONYC_OBLIGATION_SEED,
        &obligation_reserves,
    ));
    ixs.push(
        deposit_reserve_liquidity_and_obligation_collateral_v2_ix(
            user,
            onyc_reserve,
            deposit_onyc_lamports,
            caps::ONYC_OBLIGATION_SEED,
        )
        .context("build deposit_reserve_liquidity_and_obligation_collateral_v2_ix for ONyc seed")?,
    );

    Ok(ixs)
}

/// Orchestrator-driven path: convert `usdc_lamports` USDC in the daemon
/// wallet to ONyc via Jupiter, then deposit the resulting ONyc as
/// collateral. Called from `dispatch::handle_assign` when an AssignOnyc
/// carries a non-zero `usdc_lamports` field.
///
/// The two legs are submitted separately:
///   1. Jupiter swap tx — broadcasts and confirms. New ONyc balance
///      lands in the wallet's ONyc ATA.
///   2. Seed deposit bundle (this function falls through to
///      [`maybe_seed_obligation`] after the swap).
///
/// `max_slippage_bps` clamps the Jupiter quote.
pub async fn seed_with_usdc(
    ctx: &DispatchCtx,
    jup: &JupiterSwap,
    usdc_lamports: u64,
    max_slippage_bps: u16,
) -> Result<()> {
    if usdc_lamports == 0 {
        return Ok(());
    }
    let user = ctx.wallet.pubkey();

    info!(
        usdc_lamports,
        max_slippage_bps, "seed_with_usdc: building USDC→ONyc swap"
    );
    let swap_tx = build_usdc_to_onyc_swap_tx(jup, &user, usdc_lamports, max_slippage_bps)
        .await
        .context("build USDC→ONyc Jupiter swap tx")?;

    if ctx.simulate_only {
        info!("simulate-only: skipping USDC→ONyc swap broadcast");
        return Ok(());
    }

    // Submit + confirm the swap (pre-built versioned tx — sign + send).
    let sig = ctx
        .rpc
        .sign_existing_send(swap_tx, ctx.wallet.keypair())
        .await
        .context("broadcast USDC→ONyc swap tx")?;
    info!(%sig, "USDC→ONyc swap confirmed");

    // Fall through to seed-from-wallet so the new ONyc gets deposited.
    let seeded = maybe_seed_obligation(ctx).await?;
    if !seeded {
        warn!(
            "USDC→ONyc swap confirmed but seed deposit was skipped — \
             check wallet ONyc balance vs SEED_MIN_ONYC_LAMPORTS"
        );
    }
    Ok(())
}

/// Operator-trigger path: read the wallet's idle ONyc balance and
/// deposit it as collateral if any.
///
/// Returns `Ok(true)` if a deposit was executed (or simulated),
/// `Ok(false)` if there was nothing to deposit.
pub async fn maybe_seed_obligation(ctx: &DispatchCtx) -> Result<bool> {
    let user = ctx.wallet.pubkey();
    let obligation_addr = derive_user_obligation_with_seed(
        &user,
        &KAMINO_ONYC_MARKET,
        caps::ONYC_OBLIGATION_SEED.0,
        caps::ONYC_OBLIGATION_SEED.1,
    );

    let decoded = fetch_obligation(&ctx.rpc.client, &obligation_addr)
        .await
        .context("fetch obligation for seed decision")?;

    // Read wallet ONyc balance via the ATA.
    let onyc_ata = spl_associated_token_account::get_associated_token_address(&user, &ONYC_MINT);
    let onyc_balance = ctx
        .rpc
        .client
        .get_token_account_balance(&onyc_ata)
        .await
        .map(|b| b.amount.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);

    let decision = decide_seed_amount(onyc_balance, ctx.args_max_position_usdc_lamports);
    let deposit_lamports = match decision {
        SeedDecision::Deposit(n) => n,
        SeedDecision::InsufficientOnycBalance { onyc_lamports } => {
            debug!(
                onyc_lamports,
                "wallet ONyc below SEED_MIN_ONYC_LAMPORTS; nothing to seed"
            );
            return Ok(false);
        }
    };

    let user_metadata_missing = !user_metadata_exists(&ctx.rpc.client, &user).await;
    let obligation_already_exists = decoded.is_some();

    let onyc_reserve = load_reserve(
        &ctx.rpc.client,
        &KAMINO_ONYC_RESERVE,
        ONYC_MINT,
        &KAMINO_ONYC_MARKET,
    )
    .await
    .context("load ONyc reserve for seed")?;

    // Existing-position case: refresh every reserve the obligation
    // already references (mirrors multiply's rc52 fix). For a fresh
    // wallet the slice is empty.
    let mut obligation_reserve_accounts: Vec<ReserveAccounts> = Vec::new();
    if let Some(d) = decoded.as_ref() {
        for dep in &d.deposits {
            if dep.deposited_amount > 0 && dep.reserve != onyc_reserve.reserve {
                if let Ok(r) = load_reserve(
                    &ctx.rpc.client,
                    &dep.reserve,
                    Pubkey::default(),
                    &KAMINO_ONYC_MARKET,
                )
                .await
                {
                    obligation_reserve_accounts.push(r);
                }
            }
        }
        for bor in &d.borrows {
            if bor.borrowed_amount_sf > 0 && bor.reserve != onyc_reserve.reserve {
                if let Ok(r) = load_reserve(
                    &ctx.rpc.client,
                    &bor.reserve,
                    Pubkey::default(),
                    &KAMINO_ONYC_MARKET,
                )
                .await
                {
                    obligation_reserve_accounts.push(r);
                }
            }
        }
    }
    let obligation_reserve_refs: Vec<&ReserveAccounts> = obligation_reserve_accounts.iter().collect();

    let ixs = build_seed_bundle(
        &user,
        &onyc_reserve,
        deposit_lamports,
        user_metadata_missing,
        obligation_already_exists,
        &obligation_reserve_refs,
    )?;

    info!(
        deposit_lamports,
        ix_count = ixs.len(),
        simulate = ctx.simulate_only,
        "ONyc seed bundle assembled"
    );

    // No ALT for ONyc market in v0 (per KAMINO_ONYC_MARKET_LOOKUP_TABLE
    // sentinel — Kamino hasn't published one for the isolated market).
    if ctx.simulate_only {
        let res = ctx
            .rpc
            .build_sign_simulate(ixs, ctx.wallet.keypair(), SEED_CU_LIMIT, SEED_PRIORITY_FEE)
            .await
            .context("simulate ONyc seed bundle")?;
        if let Some(err) = res.err.as_ref() {
            return Err(anyhow::anyhow!("seed simulation failed: {err:?}"));
        }
        info!("ONyc seed simulation succeeded (no broadcast)");
    } else {
        let sig = ctx
            .rpc
            .build_sign_send(ixs, ctx.wallet.keypair(), SEED_CU_LIMIT, SEED_PRIORITY_FEE)
            .await
            .context("broadcast ONyc seed bundle")?;
        info!(%sig, "ONyc seed deposit confirmed");
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_seed_skips_below_dust() {
        let d = decide_seed_amount(SEED_MIN_ONYC_LAMPORTS - 1, u64::MAX);
        match d {
            SeedDecision::InsufficientOnycBalance { onyc_lamports } => {
                assert_eq!(onyc_lamports, SEED_MIN_ONYC_LAMPORTS - 1);
            }
            other => panic!("expected InsufficientOnycBalance, got {other:?}"),
        }
    }

    #[test]
    fn decide_seed_deposits_above_dust() {
        let d = decide_seed_amount(10_000_000_000, u64::MAX);
        match d {
            SeedDecision::Deposit(n) => assert_eq!(n, 10_000_000_000),
            other => panic!("expected Deposit, got {other:?}"),
        }
    }

    #[test]
    fn decide_seed_caps_at_position_max() {
        // Operator cap of $100 USDC = 100_000_000 lamports.
        // Wallet has 1_000 ONyc = 1_000_000_000_000 ONyc lamports.
        // ONyc NAV ~$1.11/token → cap_onyc ≈ 100 / 1.11 ≈ 90 ONyc ≈
        //   90_000_000_000 ONyc lamports.
        let d = decide_seed_amount(1_000_000_000_000, 100_000_000);
        match d {
            SeedDecision::Deposit(n) => {
                // Integer-divide order: `cap_usdc × 1000 / 111 × 100`. The
                // truncating divide drops a digit before the final ×100, so
                // result = 90_090_090_000, not 90_090_090_900.
                let expected = 90_090_090_000u64;
                assert_eq!(n, expected);
                assert!(n < 1_000_000_000_000);
            }
            other => panic!("expected capped Deposit, got {other:?}"),
        }
    }

    #[test]
    fn cap_helper_under_threshold_is_passthrough() {
        // 10 ONyc lamports below the cap of $1B USDC equivalent.
        let cap = cap_onyc_to_usdc_equivalent(10, 1_000_000_000_000);
        assert_eq!(cap, 10);
    }

    #[test]
    fn cap_helper_clamps_to_usdc_equivalent() {
        let cap_lamports = cap_onyc_to_usdc_equivalent(u64::MAX, 100_000_000);
        // Integer math: 100_000_000 × 1000 = 100_000_000_000;
        //               / 111 = 900_900_900 (truncates);
        //               × 100 = 90_090_090_000.
        assert_eq!(cap_lamports, 90_090_090_000);
    }
}
