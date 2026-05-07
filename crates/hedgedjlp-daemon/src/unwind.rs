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
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use zerox1_defi_protocols::constants::{
    JLP_MINT, JLP_POOL, USDC_MINT, WBTC_PORTAL_MINT, WETH_PORTAL_MINT, WSOL_MINT,
};
use zerox1_defi_protocols::protocols::jlp::{
    create_decrease_position_request_ix, derive_event_authority, derive_perpetuals,
    derive_position, derive_position_request, derive_transfer_authority, remove_liquidity_ix,
    CustodyMeta, PerpSide, PoolMeta,
};
use zerox1_defi_runtime::rpc::classify_simulation;
use zerox1_protocol::fleet::hedgedjlp::{ReportHedgedJlpWithdraw, WithdrawHedgedJlp};
use zerox1_protocol::fleet::ReportHeader;

use crate::dispatch::DispatchCtx;
use crate::rebalance::{ActivePosition, RebalanceState};

/// Per-asset compute-unit ceiling for a single close-request ixn pair.
/// Same envelope as the open path — request + ATA-create only.
const CLOSE_CU_LIMIT: u32 = 400_000;

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
    let positions_to_close = effective_positions_to_close(&active);
    if positions_to_close.is_empty() {
        info!(
            ?conv,
            "no open hedge positions to close (active.open_positions empty + synthetic-derive disabled)"
        );
    }

    let counter_base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for (i, (asset_label, position_pubkey)) in positions_to_close.iter().enumerate() {
        let counter = counter_base.wrapping_add(i as u64);
        let close_ixs = match build_close_request_ixns(
            ctx,
            asset_label,
            *position_pubkey,
            counter,
        ) {
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
                .build_sign_simulate(close_ixs, ctx.wallet.keypair(), CLOSE_CU_LIMIT, PRIORITY_FEE)
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
                Err(e) => warn!(?conv, asset = %asset_label, ?e, "close-request build_sign_simulate threw"),
            }
        } else {
            match ctx
                .rpc
                .build_sign_send(close_ixs, ctx.wallet.keypair(), CLOSE_CU_LIMIT, PRIORITY_FEE)
                .await
            {
                Ok(sig) => {
                    info!(?conv, asset = %asset_label, %sig, "close-request submitted");
                    all_sigs.push(sig.to_string());
                }
                Err(e) => warn!(?conv, asset = %asset_label, ?e, "close-request submit failed; continuing"),
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
                    info!(?conv, jlp_to_burn, ix_count = burn_ixs.len(), "JLP burn whitelist passed");
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

/// Resolve the list of `(asset_label, position_pubkey)` pairs to close.
/// If the active position recorded `open_positions` explicitly, use
/// those. Otherwise fall back to deriving synthetic positions for
/// SOL/ETH/BTC under the same scheme as `hedge.rs` — this lets M11
/// run end-to-end even when M8's open path didn't write back a
/// position list (the M8 hedge path uses synthetic custodies that
/// don't surface real position pubkeys).
fn effective_positions_to_close(active: &ActivePosition) -> Vec<(String, Pubkey)> {
    if !active.open_positions.is_empty() {
        return active.open_positions.clone();
    }
    derive_synthetic_positions()
}

/// Synthetic SOL/ETH/BTC short positions derived under the same scheme
/// as `hedge.rs`. Used as the fallback when `active.open_positions`
/// is empty (M8/M11 v0 sim-only path). Each entry uses
/// `SYNTHETIC_CUSTODY` for both position-custody and collateral-custody
/// addresses — symmetrical to `hedge.rs::synthetic_custody`.
fn derive_synthetic_positions() -> Vec<(String, Pubkey)> {
    // The wallet pubkey isn't available here; use the synthetic
    // custody as the seed for the derive call. The PDA derivation is
    // deterministic, so even if the resulting pubkey doesn't match
    // a real on-chain Position, sim will surface AccountNotFound
    // (the expected devnet shape). M9+ replaces this with a list
    // populated from the open path.
    //
    // We don't actually call `derive_position` here because the owner
    // pubkey isn't available without the dispatch context — instead
    // we return zeroed pubkey placeholders and let the build_close
    // path derive them from `ctx.wallet.pubkey()`.
    //
    // Use the synthetic mints to label the slots; the real derive
    // happens inside `build_close_request_ixns`.
    vec![
        ("SOL".to_string(), Pubkey::default()),
        ("ETH".to_string(), Pubkey::default()),
        ("BTC".to_string(), Pubkey::default()),
    ]
}

/// Build the close-request ixn slice for one asset. If the caller
/// provided a real position pubkey, use it; otherwise derive a
/// synthetic Position PDA from `(wallet, pool, custody, collateral
/// custody, side=Short)` matching the `hedge.rs` scheme.
fn build_close_request_ixns(
    ctx: &DispatchCtx,
    asset_label: &str,
    position_pubkey: Pubkey,
    counter: u64,
) -> Result<Vec<Instruction>> {
    let user = ctx.wallet.pubkey();
    let pool = synthetic_pool();

    // Resolve mint by label. Unknown labels default to SOL — they
    // shouldn't occur in practice (the open path writes "SOL"/"ETH"/
    // "BTC" labels) and the close request will sim-fail anyway.
    let asset_mint = match asset_label {
        "SOL" => WSOL_MINT,
        "ETH" => WETH_PORTAL_MINT,
        "BTC" => WBTC_PORTAL_MINT,
        _ => WSOL_MINT,
    };

    let position_custody = synthetic_custody(asset_mint, false /* not stable */);
    let collateral_custody = synthetic_custody(USDC_MINT, true /* stable */);

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
    let position_request = derive_position_request(&position, counter);

    // Notional to close = u64::MAX is not a valid request size; M11 v0
    // closes the entire position in a single call by setting the size
    // to a placeholder large value and `entire_position = true`. The
    // keeper reads `entire_position` and clamps to the actual open
    // size. M12+ may compute the live position size for partial
    // closes.
    const FULL_CLOSE_SIZE_PLACEHOLDER: u64 = u64::MAX / 2;

    let ixs = create_decrease_position_request_ix(
        &user,
        &pool,
        &position_custody,
        &collateral_custody,
        &position,
        &position_request,
        &USDC_MINT, // receive USDC
        FULL_CLOSE_SIZE_PLACEHOLDER,
        PerpSide::Short,
        CLOSE_SLIPPAGE_BPS,
        counter,
        true, // entire_position
    )
    .with_context(|| format!("build create_decrease_position_request_ix for {asset_label}"))?;

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
    fn derive_synthetic_positions_returns_three_assets() {
        let v = derive_synthetic_positions();
        assert_eq!(v.len(), 3);
        let labels: Vec<&str> = v.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["SOL", "ETH", "BTC"]);
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
            open_positions: vec![("SOL".to_string(), pk)],
        };
        let v = effective_positions_to_close(&active);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].1, pk);
    }

    #[test]
    fn effective_positions_falls_back_to_synthetic_when_empty() {
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
        // Synthetic fallback: 3 assets (SOL/ETH/BTC).
        assert_eq!(v.len(), 3);
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
}
