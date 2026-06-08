//! Simplified full-unwind for onyc-daemon (option B).
//!
//! Replaces multiply's 2138-line jitoSOL→SOL→USDC flash-loan unwind
//! with a clean ONyc→USDC flow:
//!
//!   1. read obligation state (USDC debt + ONyc collateral)
//!   2. tx A: RepayObligationLiquidityV2(USDC, u64::MAX) →
//!      WithdrawObligationCollateralAndRedeemReserveCollateralV2(ONyc, u64::MAX)
//!      — single tx, both legs run against the same in-tx-refreshed
//!      obligation state. Kamino's V2 handlers clamp internally to the
//!      actual on-chain amounts so u64::MAX safely means "as much as
//!      there is."
//!   3. tx B: Jupiter ONyc→USDC swap on the now-freed ONyc balance.
//!
//! WithdrawOnyc is always full-unwind by design — partial unwinds use a
//! lower AssignOnyc.target_ltv_bps. Mirrors multiply's WithdrawMultiply
//! contract.
//!
//! Trade-offs vs multiply's option-A:
//!   * No flash-loan path. The v2 repay handler uses the user's USDC
//!     ATA, so the wallet must hold enough USDC to cover the debt at
//!     unwind time. For v0 this means: either the operator pre-funds
//!     USDC, or previous round-trips left USDC sitting in the wallet,
//!     or the orchestrator deposits USDC via the bridge before issuing
//!     the WithdrawOnyc. Without that, the unwind returns
//!     `ERR_USDC_INSUFFICIENT_FOR_REPAY` so the orchestrator can
//!     surface the gap rather than silently sit on a stuck position.
//!     Full flash-loan unwind is option-A future work.
//!   * Dust handling: if the Jupiter swap can't route the full ONyc
//!     balance, residual ONyc is left in the wallet —
//!     `ReportOnycWithdraw.residual_onyc_lamports` surfaces it for the
//!     orchestrator to handle (manual sweep, retry next tick, etc.).

use anyhow::{Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use zerox1_defi_protocols::{
    constants::{
        KAMINO_ONYC_MARKET, KAMINO_ONYC_RESERVE, KAMINO_ONYC_USDC_RESERVE, ONYC_MINT, USDC_MINT,
    },
    protocols::{
        jupiter::{build_onyc_to_usdc_swap_tx, JupiterSwap},
        kamino::{
            derive_user_obligation_with_seed, refresh_obligation_ix, refresh_reserve_ix,
            repay_obligation_liquidity_v2_ix,
            withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix, ReserveAccounts,
        },
        kamino_loader::{fetch_obligation, load_reserve},
    },
};
use zerox1_protocol::fleet::onyc::{ReportOnycWithdraw, WithdrawOnyc};
use zerox1_protocol::fleet::ReportHeader;

use crate::caps;
use crate::dispatch::DispatchCtx;

/// Compute budget for the unwind bundle. Repay-v2 + withdraw-v2 each
/// have v2-handler farm CPIs; 800k CU is comfortable headroom.
const UNWIND_CU_LIMIT: u32 = 800_000;
const UNWIND_PRIORITY_FEE: u64 = 10_000;

/// Error codes surfaced via `ReportOnycWithdraw.header.error_code`.
pub const ERR_DEADLINE_EXPIRED: u32 = 1;
pub const ERR_OBLIGATION_NOT_FOUND: u32 = 2;
pub const ERR_USDC_INSUFFICIENT_FOR_REPAY: u32 = 3;
pub const ERR_JUPITER_SWAP_FAILED: u32 = 4;

/// Entry point. Uses the daemon's configured Jupiter client (when
/// present) for the post-unwind ONyc→USDC sweep. If no Jupiter client
/// is configured, the unwind leg still runs but the freed ONyc stays
/// in the wallet (operator manual sweep, surfaced via
/// `residual_onyc_lamports`).
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    payload: &WithdrawOnyc,
    conv: [u8; 16],
) -> Result<ReportOnycWithdraw> {
    let user = ctx.wallet.pubkey();
    let lending_market = KAMINO_ONYC_MARKET;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if payload.deadline_unix > 0 && payload.deadline_unix < now {
        return Ok(ReportOnycWithdraw {
            header: ReportHeader::err(conv, ERR_DEADLINE_EXPIRED),
            final_usdc_lamports: 0,
            residual_onyc_lamports: 0,
            tx_signatures: vec![],
        });
    }

    info!(
        simulate_only = ctx.simulate_only,
        max_slippage_bps = payload.max_slippage_bps,
        "onyc unwind entry"
    );

    let onyc_reserve = load_reserve(&ctx.rpc.client, &KAMINO_ONYC_RESERVE, ONYC_MINT, &lending_market)
        .await
        .context("load ONyc reserve for unwind")?;
    let usdc_reserve = load_reserve(
        &ctx.rpc.client,
        &KAMINO_ONYC_USDC_RESERVE,
        USDC_MINT,
        &lending_market,
    )
    .await
    .context("load USDC reserve for unwind")?;

    let obligation_addr = derive_user_obligation_with_seed(
        &user,
        &lending_market,
        caps::ONYC_OBLIGATION_SEED.0,
        caps::ONYC_OBLIGATION_SEED.1,
    );
    let decoded = match fetch_obligation(&ctx.rpc.client, &obligation_addr)
        .await
        .context("fetch obligation for unwind")?
    {
        Some(d) => d,
        None => {
            return Ok(ReportOnycWithdraw {
                header: ReportHeader::err(conv, ERR_OBLIGATION_NOT_FOUND),
                final_usdc_lamports: 0,
                residual_onyc_lamports: 0,
                tx_signatures: vec![],
            });
        }
    };

    // Does the wallet hold enough USDC to cover the repay? v0 assumes
    // operator pre-funds OR previous round-trips left USDC sitting.
    // Surface the gap if neither.
    let usdc_owed_micro: u128 = decoded
        .borrows
        .iter()
        .filter(|b| b.reserve == KAMINO_ONYC_USDC_RESERVE)
        .map(|b| b.borrowed_amount_sf >> 60)
        .sum();
    let usdc_owed_lamports = u64::try_from(usdc_owed_micro).unwrap_or(u64::MAX);

    let usdc_ata = get_associated_token_address(&user, &USDC_MINT);
    let usdc_balance = ctx
        .rpc
        .client
        .get_token_account_balance(&usdc_ata)
        .await
        .map(|b| b.amount.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);

    if usdc_balance < usdc_owed_lamports {
        warn!(
            usdc_owed_lamports,
            usdc_balance,
            "wallet USDC insufficient to cover repay — operator must pre-fund (v0 has no flash-loan path)"
        );
        return Ok(ReportOnycWithdraw {
            header: ReportHeader::err(conv, ERR_USDC_INSUFFICIENT_FOR_REPAY),
            final_usdc_lamports: 0,
            residual_onyc_lamports: 0,
            tx_signatures: vec![],
        });
    }

    let mut tx_signatures: Vec<String> = Vec::new();

    // Phase 1: unwind bundle (repay USDC + withdraw all ONyc).
    let ixs = build_unwind_bundle(&user, &onyc_reserve, &usdc_reserve, u64::MAX, u64::MAX)?;

    if ctx.simulate_only {
        let res = ctx
            .rpc
            .build_sign_simulate(ixs, ctx.wallet.keypair(), UNWIND_CU_LIMIT, UNWIND_PRIORITY_FEE)
            .await
            .context("simulate ONyc unwind bundle")?;
        if let Some(err) = res.err.as_ref() {
            return Err(anyhow::anyhow!("unwind simulation failed: {err:?}"));
        }
        info!("ONyc unwind simulation succeeded (no broadcast)");
        return Ok(ReportOnycWithdraw {
            header: ReportHeader::ok(conv),
            final_usdc_lamports: 0,
            residual_onyc_lamports: 0,
            tx_signatures: vec![],
        });
    }

    let sig = ctx
        .rpc
        .build_sign_send(ixs, ctx.wallet.keypair(), UNWIND_CU_LIMIT, UNWIND_PRIORITY_FEE)
        .await
        .context("broadcast ONyc unwind bundle")?;
    info!(%sig, "ONyc unwind (repay + withdraw) confirmed");
    tx_signatures.push(sig.to_string());

    // Phase 2: Jupiter ONyc→USDC swap on the freed ONyc balance.
    let onyc_ata = get_associated_token_address(&user, &ONYC_MINT);
    let onyc_balance = ctx
        .rpc
        .client
        .get_token_account_balance(&onyc_ata)
        .await
        .map(|b| b.amount.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);

    let jup = match ctx.jupiter.as_ref() {
        Some(j) => j,
        None => {
            info!(
                onyc_balance,
                "no Jupiter configured — leaving freed ONyc in wallet (operator sweep)"
            );
            return Ok(ReportOnycWithdraw {
                header: ReportHeader::ok(conv),
                final_usdc_lamports: 0,
                residual_onyc_lamports: onyc_balance,
                tx_signatures,
            });
        }
    };

    if onyc_balance == 0 {
        return Ok(ReportOnycWithdraw {
            header: ReportHeader::ok(conv),
            final_usdc_lamports: 0,
            residual_onyc_lamports: 0,
            tx_signatures,
        });
    }

    let swap_tx =
        match build_onyc_to_usdc_swap_tx(jup, &user, onyc_balance, payload.max_slippage_bps).await {
            Ok(tx) => tx,
            Err(e) => {
                warn!(?e, onyc_balance, "Jupiter quote for ONyc→USDC failed");
                return Ok(ReportOnycWithdraw {
                    header: ReportHeader::err(conv, ERR_JUPITER_SWAP_FAILED),
                    final_usdc_lamports: 0,
                    residual_onyc_lamports: onyc_balance,
                    tx_signatures,
                });
            }
        };

    let swap_sig = ctx
        .rpc
        .sign_existing_send(swap_tx, ctx.wallet.keypair())
        .await
        .context("broadcast ONyc→USDC swap")?;
    info!(%swap_sig, "ONyc→USDC swap confirmed");
    tx_signatures.push(swap_sig.to_string());

    // Re-read balances for the report.
    let final_usdc = ctx
        .rpc
        .client
        .get_token_account_balance(&usdc_ata)
        .await
        .map(|b| b.amount.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);
    let residual_onyc = ctx
        .rpc
        .client
        .get_token_account_balance(&onyc_ata)
        .await
        .map(|b| b.amount.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);

    Ok(ReportOnycWithdraw {
        header: ReportHeader::ok(conv),
        final_usdc_lamports: final_usdc,
        residual_onyc_lamports: residual_onyc,
        tx_signatures,
    })
}

/// Build the single-tx unwind bundle.
///
///   1. RefreshReserve(ONyc)
///   2. RefreshReserve(USDC)
///   3. RefreshObligation
///   4. RepayObligationLiquidityV2(USDC, u64::MAX → all debt)
///   5. WithdrawObligationCollateralAndRedeemReserveCollateralV2(ONyc, u64::MAX)
///
/// Kamino's V2 handlers clamp `u64::MAX` to the actual on-chain amount
/// internally (per rc19 documentation in the multiply unwind), so
/// passing u64::MAX is the canonical "all-of-it" sentinel.
pub fn build_unwind_bundle(
    user: &Pubkey,
    onyc_reserve: &ReserveAccounts,
    usdc_reserve: &ReserveAccounts,
    repay_amount: u64,
    withdraw_amount: u64,
) -> Result<Vec<Instruction>> {
    let mut ixs = Vec::with_capacity(5);
    ixs.push(refresh_reserve_ix(onyc_reserve));
    ixs.push(refresh_reserve_ix(usdc_reserve));
    let obligation_reserves = vec![onyc_reserve.reserve, usdc_reserve.reserve];
    ixs.push(refresh_obligation_ix(
        user,
        &onyc_reserve.lending_market,
        caps::ONYC_OBLIGATION_SEED,
        &obligation_reserves,
    ));
    ixs.push(repay_obligation_liquidity_v2_ix(
        user,
        usdc_reserve,
        repay_amount,
        caps::ONYC_OBLIGATION_SEED,
    )?);
    ixs.push(withdraw_obligation_collateral_and_redeem_reserve_collateral_v2_ix(
        user,
        onyc_reserve,
        withdraw_amount,
        caps::ONYC_OBLIGATION_SEED,
    )?);
    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_reserve(market: Pubkey, mint: Pubkey) -> ReserveAccounts {
        ReserveAccounts {
            reserve: Pubkey::new_unique(),
            lending_market: market,
            lending_market_authority: Pubkey::new_unique(),
            liquidity_mint: mint,
            liquidity_supply: Pubkey::new_unique(),
            collateral_mint: Pubkey::new_unique(),
            collateral_supply: Pubkey::new_unique(),
            fee_receiver: Pubkey::new_unique(),
            scope_prices: Pubkey::new_unique(),
            farm_collateral: Pubkey::default(),
            farm_debt: Pubkey::default(),
        }
    }

    #[test]
    fn unwind_bundle_has_five_ixs() {
        let market = Pubkey::new_unique();
        let onyc = dummy_reserve(market, ONYC_MINT);
        let usdc = dummy_reserve(market, USDC_MINT);
        let user = Pubkey::new_unique();
        let ixs = build_unwind_bundle(&user, &onyc, &usdc, u64::MAX, u64::MAX).unwrap();
        assert_eq!(
            ixs.len(),
            5,
            "expected 5 ixs: 2× refresh_reserve, refresh_obligation, repay, withdraw"
        );
    }

    #[test]
    fn unwind_bundle_uses_lending_program() {
        // Sanity: every ix in the bundle targets the Kamino Lend program.
        let market = Pubkey::new_unique();
        let onyc = dummy_reserve(market, ONYC_MINT);
        let usdc = dummy_reserve(market, USDC_MINT);
        let user = Pubkey::new_unique();
        let ixs = build_unwind_bundle(&user, &onyc, &usdc, u64::MAX, u64::MAX).unwrap();
        use zerox1_defi_protocols::constants::KAMINO_LEND_PROGRAM_ID;
        for (i, ix) in ixs.iter().enumerate() {
            assert_eq!(
                ix.program_id, KAMINO_LEND_PROGRAM_ID,
                "ix {i} should target Kamino Lend program"
            );
        }
    }

    #[test]
    fn unwind_bundle_rejects_zero_repay() {
        let market = Pubkey::new_unique();
        let onyc = dummy_reserve(market, ONYC_MINT);
        let usdc = dummy_reserve(market, USDC_MINT);
        let user = Pubkey::new_unique();
        let err = build_unwind_bundle(&user, &onyc, &usdc, 0, u64::MAX).unwrap_err();
        assert!(
            format!("{err:?}").to_lowercase().contains("zero"),
            "expected ZeroAmount-style error, got {err:?}"
        );
    }

    #[test]
    fn unwind_bundle_rejects_zero_withdraw() {
        let market = Pubkey::new_unique();
        let onyc = dummy_reserve(market, ONYC_MINT);
        let usdc = dummy_reserve(market, USDC_MINT);
        let user = Pubkey::new_unique();
        let err = build_unwind_bundle(&user, &onyc, &usdc, u64::MAX, 0).unwrap_err();
        assert!(
            format!("{err:?}").to_lowercase().contains("zero"),
            "expected ZeroAmount-style error, got {err:?}"
        );
    }

    #[test]
    fn error_codes_are_distinct() {
        let codes = [
            ERR_DEADLINE_EXPIRED,
            ERR_OBLIGATION_NOT_FOUND,
            ERR_USDC_INSUFFICIENT_FOR_REPAY,
            ERR_JUPITER_SWAP_FAILED,
        ];
        let mut sorted = codes;
        sorted.sort();
        for w in sorted.windows(2) {
            assert!(w[0] != w[1], "duplicate error code: {}", w[0]);
        }
    }
}
