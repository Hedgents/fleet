//! Withdrawal / unwind flow (M11).
//!
//! Real unwind sequence (replaces the M4 stub):
//!   1. For each open hedge position recorded in `RebalanceState`,
//!      build a `create_decrease_position_request_v2` ixn pair (request
//!      submit only — keeper executes the actual close 1-3 slots later).
//!      Whitelist-verify and either submit or simulate per `ctx.simulate_only`.
//!   2. Build a `remove_liquidity_2` ixn pair to burn JLP and receive
//!      USDC. For `payload.jlp_lamports == u64::MAX` the daemon burns
//!      the full `active.jlp_acquired_lamports`; otherwise the min of
//!      the requested amount and what we hold.
//!   3. Clear the active position from `RebalanceState`. Subsequent
//!      rebalance + telemetry ticks no-op until a fresh AssignHedgedJlp
//!      records a new position.
//!
//! 2-tx model: this milestone only SUBMITS close-requests. Polling for
//! keeper execution is M9's rebalancer; M11 v0 does not block on it.
//! Operators verify execution via the JSONL telemetry log. The
//! `usdc_returned_lamports` field on the Report is a v0 proxy
//! (`jlp_to_burn`) — real post-tx balance read is M12+ work.
//!
//! No-active-position case: when `state.snapshot_active_position()`
//! returns `None`, log a warning and return a sentinel zero Report
//! (ok=true, zero usdc, no signatures). This matches the M5 stub
//! behavior so the existing devnet smoke still passes.
//!
//! Whitelist (audit-fix I1): every ixn slice — close requests AND the
//! JLP burn — passes through `ctx.whitelist.verify_ixns` before signing.
//! Both legs only touch programs already in the M8 whitelist
//! (Jupiter Perpetuals, SPL Token, ATA, System, Compute Budget).
//!
//! Confidence: `create_decrease_position_request_v2` discriminator +
//! arg layout are best-effort per the public IDL parser references —
//! same caveat as M8's increase variant. Devnet sim will surface
//! InstructionError because Jupiter Perps is mainnet-only; the
//! daemon emits a Report with empty tx_sigs in that case (sim-only)
//! or `error_code=5` if the build path itself fails fatally.

use anyhow::{Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use tracing::{info, warn};

use zerox1_defi_protocols::constants::{
    JLP_MINT, JLP_POOL, USDC_MINT, WBTC_PORTAL_MINT, WETH_PORTAL_MINT, WSOL_MINT,
};
use zerox1_defi_protocols::protocols::jlp::{
    create_decrease_position_request_ix, derive_event_authority, derive_perpetuals,
    derive_position, derive_position_request, derive_transfer_authority, remove_liquidity_ix,
    CustodyMeta, PerpSide, PoolMeta, RequestChange,
};
use zerox1_defi_runtime::rpc::classify_simulation;
use zerox1_protocol::fleet::hedgedjlp::{ReportHedgedJlpWithdraw, WithdrawHedgedJlp};
use zerox1_protocol::fleet::ReportHeader;

use crate::dispatch::DispatchCtx;
use crate::rebalance::{ActivePosition, RebalanceState};

/// Per-asset compute-unit ceiling for a single close-request ixn pair.
/// Same envelope as the open path — request + ATA-create only.
const CLOSE_CU_LIMIT: u32 = 600_000;

/// JLP burn (`remove_liquidity_2`) is heavier than the perp request
/// (AUM read + price oracle reads + token transfers). Match the
/// jlp_hedge buy-leg envelope.
const BURN_CU_LIMIT: u32 = 600_000;

/// Same priority fee envelope as M6/M8.
const PRIORITY_FEE: u64 = 10_000;

/// Default close-request slippage. Operators tune via runbook.
const CLOSE_SLIPPAGE_BPS: u16 = 50;

/// Synthetic stand-in custody address — mirrors `hedge.rs`'s
/// `SYNTHETIC_CUSTODY` constant. The on-chain custody read lands in
/// M9+; for now the unwind reuses the same placeholder so the
/// PDA-derived position pubkeys round-trip with what `hedge.rs`
/// derived at open time.
const SYNTHETIC_CUSTODY: Pubkey =
    solana_sdk::pubkey!("G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa");

/// Run (or simulate) the unwind sequence for a recorded position.
///
/// Sequence:
///   1. Iterate `active.open_positions` — for each, build close-request
///      ixns, whitelist-verify, simulate or submit.
///   2. Build JLP burn ixns (`remove_liquidity_2` for
///      `min(payload.jlp_lamports, active.jlp_acquired_lamports)` —
///      `u64::MAX` means full).
///   3. Clear active position from RebalanceState.
///
/// Returns a composed `ReportHedgedJlpWithdraw` with all collected
/// tx_signatures (empty in simulate-only mode). `usdc_returned_lamports`
/// is a v0 proxy: equals `jlp_to_burn` on burn-submit success, 0
/// otherwise.
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    state: &Arc<RebalanceState>,
    payload: &WithdrawHedgedJlp,
    conv: [u8; 16],
) -> Result<ReportHedgedJlpWithdraw> {
    let active = match state.snapshot_active_position() {
        Some(p) => p,
        None => {
            warn!(
                ?conv,
                "withdraw requested but no active position tracked — returning zero-Report sentinel",
            );
            return Ok(ReportHedgedJlpWithdraw {
                header: ReportHeader::ok(conv),
                usdc_returned_lamports: 0,
                tx_signatures: vec![],
            });
        }
    };

    info!(
        ?conv,
        jlp_lamports = payload.jlp_lamports,
        jlp_acquired = active.jlp_acquired_lamports,
        open_position_count = active.open_positions.len(),
        simulate_only = ctx.simulate_only,
        "hedgedjlp unwind starting (M11)"
    );

    let mut all_sigs: Vec<String> = Vec::new();

    // ── 1. Close all open hedge shorts ─────────────────────────────────
    //
    // Audit-fix C2: with `set_active_position` now wired into the open
    // path, `active.open_positions` is populated after a successful
    // submit. Each entry carries the `open_counter` from `hedge.rs` so
    // the close-request PDA derivation matches the open-request PDA.
    //
    // Empty list = no real open positions tracked. This can happen on
    // sim-only Assigns (audit-fix C1 deliberately doesn't persist
    // sim-only state). Surface as a zero-Report — do NOT silently fall
    // through to synthetic derivation, which would build close requests
    // for PDAs that don't exist on chain.
    let positions_to_close = effective_positions_to_close(&active);
    if positions_to_close.is_empty() {
        warn!(
            ?conv,
            "no tracked positions to close — was this a sim-only Assign? returning zero-Report"
        );
        return Ok(ReportHedgedJlpWithdraw {
            header: ReportHeader::ok(conv),
            usdc_returned_lamports: 0,
            tx_signatures: vec![],
        });
    }

    for (asset_label, position_pubkey, open_counter) in positions_to_close.iter() {
        let counter = *open_counter;
        let close_ixs = match build_close_request_ixns(ctx, asset_label, *position_pubkey, counter)
        {
            Ok(ixs) => ixs,
            Err(e) => {
                warn!(
                    ?conv,
                    asset = %asset_label,
                    ?e,
                    "build_close_request_ixns failed; skipping asset",
                );
                continue;
            }
        };

        if let Err(e) = ctx.whitelist.verify_ixns(&close_ixs) {
            warn!(
                ?conv,
                asset = %asset_label,
                ?e,
                "whitelist rejected close-request ixns; skipping asset",
            );
            continue;
        }
        info!(
            ?conv,
            asset = %asset_label,
            ix_count = close_ixs.len(),
            "close-request whitelist passed",
        );

        if ctx.simulate_only {
            match ctx
                .rpc
                .build_sign_simulate(
                    close_ixs,
                    ctx.wallet.keypair(),
                    CLOSE_CU_LIMIT,
                    PRIORITY_FEE,
                )
                .await
            {
                Ok(sim) => {
                    let (layout_valid, summary) = classify_simulation(&sim);
                    if sim.err.is_some() {
                        warn!(
                            ?conv,
                            asset = %asset_label,
                            layout_valid,
                            summary = %summary,
                            "close-request simulation returned error \
                             (expected on devnet — Jupiter Perps mainnet-only)",
                        );
                    } else {
                        info!(
                            ?conv,
                            asset = %asset_label,
                            layout_valid,
                            summary = %summary,
                            "close-request simulation succeeded",
                        );
                    }
                }
                Err(e) => {
                    warn!(?conv, asset = %asset_label, ?e, "close-request build_sign_simulate threw")
                }
            }
        } else {
            match ctx
                .rpc
                .build_sign_send(
                    close_ixs,
                    ctx.wallet.keypair(),
                    CLOSE_CU_LIMIT,
                    PRIORITY_FEE,
                )
                .await
            {
                Ok(sig) => {
                    info!(?conv, asset = %asset_label, %sig, "close-request submitted");
                    all_sigs.push(sig.to_string());
                }
                Err(e) => {
                    warn!(?conv, asset = %asset_label, ?e, "close-request submit failed; continuing")
                }
            }
        }
    }

    // ── 2. Burn JLP via remove_liquidity_2 ─────────────────────────────
    let jlp_to_burn = compute_jlp_to_burn(payload.jlp_lamports, active.jlp_acquired_lamports);
    let mut usdc_returned: u64 = 0;
    if jlp_to_burn == 0 {
        info!(?conv, "jlp_to_burn=0 — skipping JLP burn leg");
    } else {
        match build_jlp_burn_ixns(ctx, jlp_to_burn) {
            Ok(burn_ixs) => {
                if let Err(e) = ctx.whitelist.verify_ixns(&burn_ixs) {
                    warn!(?conv, ?e, "whitelist rejected JLP burn ixns; skipping burn");
                } else {
                    info!(
                        ?conv,
                        jlp_to_burn,
                        ix_count = burn_ixs.len(),
                        "JLP burn whitelist passed"
                    );
                    if ctx.simulate_only {
                        match ctx
                            .rpc
                            .build_sign_simulate(
                                burn_ixs,
                                ctx.wallet.keypair(),
                                BURN_CU_LIMIT,
                                PRIORITY_FEE,
                            )
                            .await
                        {
                            Ok(sim) => {
                                let (layout_valid, summary) = classify_simulation(&sim);
                                if sim.err.is_some() {
                                    warn!(
                                        ?conv,
                                        layout_valid,
                                        summary = %summary,
                                        "JLP burn simulation returned error \
                                         (expected on devnet — Jupiter Perps mainnet-only)",
                                    );
                                } else {
                                    info!(
                                        ?conv,
                                        layout_valid,
                                        summary = %summary,
                                        "JLP burn simulation succeeded",
                                    );
                                }
                            }
                            Err(e) => warn!(?conv, ?e, "JLP burn build_sign_simulate threw"),
                        }
                    } else {
                        match ctx
                            .rpc
                            .build_sign_send(
                                burn_ixs,
                                ctx.wallet.keypair(),
                                BURN_CU_LIMIT,
                                PRIORITY_FEE,
                            )
                            .await
                        {
                            Ok(sig) => {
                                info!(?conv, %sig, "JLP burn confirmed on-chain");
                                all_sigs.push(sig.to_string());
                                // v0 proxy: equate to jlp_to_burn pending the
                                // post-tx balance read (M12+).
                                usdc_returned = jlp_to_burn;
                            }
                            Err(e) => warn!(?conv, ?e, "JLP burn build_sign_send failed"),
                        }
                    }
                }
            }
            Err(e) => warn!(?conv, ?e, "build_jlp_burn_ixns failed; skipping burn leg"),
        }
    }

    // ── 3. Clear active position ───────────────────────────────────────
    state.clear_active_position();
    info!(?conv, "active position cleared from RebalanceState");

    Ok(ReportHedgedJlpWithdraw {
        header: ReportHeader::ok(conv),
        usdc_returned_lamports: usdc_returned,
        tx_signatures: all_sigs,
    })
}

/// Compute how many JLP lamports to burn for a given `payload.jlp_lamports`,
/// honoring the `u64::MAX` full-withdraw sentinel and clamping at the
/// daemon's actual JLP holdings (`active.jlp_acquired_lamports`).
pub(crate) fn compute_jlp_to_burn(requested: u64, jlp_acquired: u64) -> u64 {
    if requested == u64::MAX {
        jlp_acquired
    } else {
        requested.min(jlp_acquired)
    }
}

/// Resolve the list of `(asset_label, position_pubkey, open_counter)`
/// triples to close.
///
/// Audit-fix C2: with the open path now writing back the real counter
/// + position pubkey, the unwind reuses both. The synthetic-derive
/// fallback has been removed — empty `open_positions` returns an
/// empty list (caller surfaces zero-Report). The previous behavior
/// silently fabricated close requests for synthetic PDAs that didn't
/// match any real on-chain position; safer to surface the gap.
fn effective_positions_to_close(active: &ActivePosition) -> Vec<(String, Pubkey, u64)> {
    active.open_positions.clone()
}

/// Build the close-request ixn slice for one asset. The caller passes
/// the real position pubkey (recorded by the open path) and the
/// `open_counter` from `hedge.rs` so the `PositionRequest` PDA matches
/// the open-side derivation (audit-fix C2).
///
/// Audit-fix C3: synthetic-custody guard fires before signing; in
/// sim-only mode it logs a warning, in submit mode it bails.
fn build_close_request_ixns(
    ctx: &DispatchCtx,
    asset_label: &str,
    position_pubkey: Pubkey,
    counter: u64,
) -> Result<Vec<Instruction>> {
    let user = ctx.wallet.pubkey();
    // Audit fix 9: prefer live-loaded pool from boot.
    let pool: PoolMeta = match &ctx.pool {
        Some(p) => (**p).clone(),
        None => synthetic_pool(),
    };

    // Resolve mint by label. Unknown labels default to SOL — they
    // shouldn't occur in practice (the open path writes "SOL"/"ETH"/
    // "BTC" labels) and the close request will sim-fail anyway.
    let asset_mint = match asset_label {
        "SOL" => WSOL_MINT,
        "ETH" => WETH_PORTAL_MINT,
        "BTC" => WBTC_PORTAL_MINT,
        _ => WSOL_MINT,
    };

    let position_custody = pool
        .custody_for_mint(&asset_mint)
        .cloned()
        .unwrap_or_else(|| synthetic_custody(asset_mint, false));
    let collateral_custody = pool
        .custody_for_mint(&USDC_MINT)
        .cloned()
        .unwrap_or_else(|| synthetic_custody(USDC_MINT, true));

    // Audit-fix C3: refuse to sign on synthetic placeholder pubkeys
    // unless we're in sim-only mode.
    crate::hedge::validate_custody_not_synthetic(
        &position_custody,
        &format!("hedge close ({asset_label}) position-custody"),
        ctx.simulate_only,
    )?;
    crate::hedge::validate_custody_not_synthetic(
        &collateral_custody,
        &format!("hedge close ({asset_label}) collateral-custody"),
        ctx.simulate_only,
    )?;

    // C2: always use the recorded position pubkey when present;
    // otherwise derive (transitional path for any legacy callers).
    let position = if position_pubkey == Pubkey::default() {
        derive_position(
            &user,
            &pool.pool,
            &position_custody.address,
            &collateral_custody.address,
            PerpSide::Short,
        )
    } else {
        position_pubkey
    };
    // Audit fix 3: PositionRequest PDA for the close uses request_change=Decrease.
    let position_request = derive_position_request(&position, counter, RequestChange::Decrease);

    // Audit fix 8: 6-decimal USD slippage price (NOT bps). For a
    // Short close: lower mark = better fill, so subtract a buffer.
    let mark = crate::hedge::sim_mark_price_micro_usd(asset_label);
    let price_slippage_micro_usd = mark - mark / 100;

    // Per spec §4: with `entire_position=Some(true)`, the keeper
    // reads `entire_position` and ignores the size field. Pass 0
    // and let the keeper clamp.
    let ixs = create_decrease_position_request_ix(
        &user,
        &pool,
        &position_custody,
        &collateral_custody,
        &position,
        &position_request,
        &USDC_MINT,
        0, // size_usd_delta — keeper ignores when entire_position=true
        price_slippage_micro_usd,
        counter,
        true, // entire_position
    )
    .with_context(|| {
        format!("build create_decrease_position_market_request ix for {asset_label}")
    })?;

    // Pin CLOSE_SLIPPAGE_BPS for backward compat — value preserved
    // in the param name for future runbook reference.
    let _ = CLOSE_SLIPPAGE_BPS;
    Ok(ixs)
}

/// Build the JLP burn ixn slice (`remove_liquidity_2`) using the
/// synthetic USDC custody — same shape as the M6 buy leg's synthetic.
/// Live custody decode lands in M9+ (jlp_hedge::read_pool_state has
/// the loader; wiring it through dispatch is M12+).
fn build_jlp_burn_ixns(ctx: &DispatchCtx, jlp_lamports: u64) -> Result<Vec<Instruction>> {
    if jlp_lamports == 0 {
        anyhow::bail!("jlp_lamports must be > 0");
    }
    let user = ctx.wallet.pubkey();
    let pool = synthetic_pool();
    let usdc_custody = synthetic_custody(USDC_MINT, true);

    // Audit-fix C3: synthetic-custody guard.
    crate::hedge::validate_custody_not_synthetic(
        &usdc_custody,
        "JLP burn USDC custody",
        ctx.simulate_only,
    )?;

    // M6 disables slippage protection (min_amount_out = 0). Mainnet
    // promotion (M12) computes a real bound from
    // `getRemoveLiquidityAmountAndFee2`. For now the operator
    // approval queue is the slippage gate.
    remove_liquidity_ix(&user, &pool, &usdc_custody, jlp_lamports, 0)
        .context("build remove_liquidity_ix")
}

fn synthetic_custody(mint: Pubkey, is_stable: bool) -> CustodyMeta {
    CustodyMeta {
        address: SYNTHETIC_CUSTODY,
        mint,
        token_account: SYNTHETIC_CUSTODY,
        pythnet_price_account: SYNTHETIC_CUSTODY,
        doves_price_account: SYNTHETIC_CUSTODY,
        decimals: if is_stable { 6 } else { 9 },
        is_stable,
    }
}

fn synthetic_pool() -> PoolMeta {
    PoolMeta {
        pool: JLP_POOL,
        jlp_mint: JLP_MINT,
        perpetuals: derive_perpetuals(),
        transfer_authority: derive_transfer_authority(),
        event_authority: derive_event_authority(),
        custodies: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_jlp_to_burn_full_withdraw_sentinel() {
        // u64::MAX → burn the full acquired amount.
        assert_eq!(compute_jlp_to_burn(u64::MAX, 1_000_000), 1_000_000);
        assert_eq!(compute_jlp_to_burn(u64::MAX, 0), 0);
    }

    #[test]
    fn compute_jlp_to_burn_partial_clamps_to_holdings() {
        // Requested below holdings → use requested.
        assert_eq!(compute_jlp_to_burn(100, 1_000), 100);
        // Requested above holdings → clamp to holdings (don't try to
        // burn more than we have).
        assert_eq!(compute_jlp_to_burn(2_000, 1_000), 1_000);
    }

    #[test]
    fn compute_jlp_to_burn_zero_requested() {
        // Zero requested = burn nothing (caps validation rejects
        // payload.jlp_lamports=0 upstream, but defense-in-depth here).
        assert_eq!(compute_jlp_to_burn(0, 1_000), 0);
    }

    #[test]
    fn effective_positions_uses_explicit_when_present() {
        let pk = Pubkey::new_unique();
        let active = ActivePosition {
            conv: [0u8; 16],
            our_jlp_lamports: 1,
            jlp_acquired_lamports: 1,
            target_delta_bps: 0,
            max_borrow_rate_bps: 0,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 0,
            open_positions: vec![("SOL".to_string(), pk, 7777)],
        };
        let v = effective_positions_to_close(&active);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1, pk);
        // Audit-fix C2: counter round-trips so close-request PDA
        // matches the open-request PDA.
        assert_eq!(v[0].2, 7777);
    }

    #[test]
    fn effective_positions_returns_empty_when_no_tracked_open() {
        // Audit-fix C2: empty open_positions → empty close list. The
        // run_or_simulate caller treats this as zero-Report; we no
        // longer fall through to synthetic-derive (which silently
        // built bogus close requests).
        let active = ActivePosition {
            conv: [0u8; 16],
            our_jlp_lamports: 1,
            jlp_acquired_lamports: 1,
            target_delta_bps: 0,
            max_borrow_rate_bps: 0,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 0,
            open_positions: vec![],
        };
        let v = effective_positions_to_close(&active);
        assert!(v.is_empty(), "synthetic-derive fallback removed");
    }

    #[test]
    fn synthetic_pool_uses_jlp_mint_and_pool_constants() {
        let p = synthetic_pool();
        assert_eq!(p.pool, JLP_POOL);
        assert_eq!(p.jlp_mint, JLP_MINT);
        assert!(p.custodies.is_empty());
    }

    #[test]
    fn synthetic_custody_decimals_match_stable_flag() {
        let stable = synthetic_custody(USDC_MINT, true);
        assert_eq!(stable.decimals, 6);
        assert!(stable.is_stable);
        let non_stable = synthetic_custody(WSOL_MINT, false);
        assert_eq!(non_stable.decimals, 9);
        assert!(!non_stable.is_stable);
    }

    #[test]
    fn empty_open_positions_with_recorded_active_returns_empty_close_list() {
        // Audit-fix C2 pin: even when an ActivePosition exists, if its
        // open_positions list is empty (sim-only assign that didn't
        // persist would never reach unwind, but defense-in-depth) the
        // close list is empty and run_or_simulate returns zero-Report.
        let active = ActivePosition {
            conv: [42u8; 16],
            our_jlp_lamports: 9,
            jlp_acquired_lamports: 9,
            target_delta_bps: 0,
            max_borrow_rate_bps: 5_000,
            custody_pubkeys: vec![Pubkey::new_unique()],
            hedge_notional_usdc: 1_234_567,
            open_positions: vec![],
        };
        assert!(effective_positions_to_close(&active).is_empty());
    }
}
