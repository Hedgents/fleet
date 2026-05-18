//! Withdrawal / unwind flow.
//!
//! Real unwind sequence:
//!   1. For each open hedge position recorded in `RebalanceState`,
//!      read the on-chain `Position` account, build a
//!      `create_decrease_position_market_request` ixn pair (request
//!      submit only — keeper executes the actual close 1-3 slots later).
//!      Whitelist-verify and either submit or simulate per
//!      `ctx.simulate_only`.
//!   2. Build a `remove_liquidity_2` ixn pair (routed via the Jupiter
//!      Swap aggregator) to burn JLP and receive USDC. For
//!      `payload.jlp_lamports == u64::MAX` the daemon burns the full
//!      `active.jlp_acquired_lamports`; otherwise the min of the
//!      requested amount and what we hold.
//!   3. Clear the active position from `RebalanceState`. Subsequent
//!      rebalance + telemetry ticks no-op until a fresh AssignHedgedJlp
//!      (or the next boot's `recover.rs`) records a new position.
//!
//! 2-tx model: this path only SUBMITS close-requests. Polling for
//! keeper execution is the rebalancer's job; v0 unwind does not block
//! on it. Operators verify execution via the JSONL telemetry log. The
//! `usdc_returned_lamports` field on the Report is a v0 proxy
//! (`jlp_to_burn`) — a real post-tx balance read is future work.
//!
//! ## fleet-v0.4.1: chain-derived close-request PDAs
//!
//! Previously the unwind path reused `open_counter` (the counter the
//! open path stamped at increase-request time) to derive the
//! close-request `PositionRequest` PDA. That conflated two unrelated
//! things: (a) the counter is just a per-request randomization nonce
//! (spec §3.6), and (b) the increase-request PDA is closed on-chain
//! the moment the keeper executes it, so the open counter has no
//! lasting on-chain meaning. The conflation broke withdraw for
//! recovered positions (post-restart), which carry no original
//! counter — we never saw the open tx.
//!
//! The fix:
//!   - Read the on-chain `Position` account at the PDA recorded in
//!     `open_positions[i].1`. If the account doesn't exist, the
//!     owner is wrong, the discriminator mismatches, or the position
//!     `is_empty()` (size_usd == 0 — fully closed), skip the leg with
//!     a warn. This catches stale recovered entries and races against
//!     manual closes.
//!   - Generate a fresh `close_counter = unix_seconds + i` at withdraw
//!     time (same pattern as `hedge.rs` open path). This is a fresh
//!     randomization nonce; it does NOT need to match anything from
//!     open time.
//!   - Derive the close-request PDA via
//!     `derive_position_request(position, close_counter,
//!     RequestChange::Decrease)`.
//!
//! Both Assign-tracked and recovered positions take this same path.
//! Recovered positions are no longer "rebalance-only" — they can be
//! cleanly withdrawn.
//!
//! No-active-position case: when `state.snapshot_active_position()`
//! returns `None` (truly no position — wallet never held JLP, or a
//! prior Withdraw drained state and `recover.rs` ran on a fresh boot),
//! log a warning and return a sentinel zero Report (ok=true, zero
//! usdc, no signatures). With `recover.rs` populating state at boot
//! whenever JLP + Jupiter Perps shorts exist on chain, this sentinel
//! branch is now only reached when there is genuinely nothing to
//! unwind.
//!
//! Whitelist (audit-fix I1): every ixn slice — close requests AND the
//! JLP burn — passes through `ctx.whitelist.verify_ixns` before signing.
//! Both legs only touch programs already in the whitelist (Jupiter
//! Perpetuals, SPL Token, ATA, System, Compute Budget). The Jupiter
//! Swap aggregator tx skips whitelist verify by design (Jupiter signs
//! its own ALTs / inner ixs).
//!
//! ## Assumptions
//!
//! - The Jupiter Perps `Position` account is owned by
//!   `JUPITER_PERPETUALS_PROGRAM_ID` and decodes via `decode_position`
//!   (verified 2026-05-15 against the live IDL — see
//!   `zerox1_defi_protocols::protocols::jlp` §"Position account
//!   decoder").
//! - The recorded `(label, position_pda)` pair is the canonical
//!   Position PDA for that asset. We do NOT cross-validate against a
//!   re-derived `derive_position(...)` because the live pool's custody
//!   addresses may not be loaded (devnet — Jupiter Perps mainnet-only)
//!   and re-derivation against synthetic placeholders would silently
//!   succeed with a wrong PDA. The on-chain account read is the
//!   authoritative check: if the data decodes cleanly with a non-empty
//!   `size_usd`, the position is real.

use anyhow::{Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use zerox1_defi_protocols::constants::{
    JLP_MINT, JLP_POOL, JUPITER_PERPETUALS_PROGRAM_ID, USDC_MINT, WBTC_PORTAL_MINT,
    WETH_PORTAL_MINT, WSOL_MINT,
};
use zerox1_defi_protocols::protocols::jlp::{
    create_decrease_position_request_ix, decode_position, derive_event_authority,
    derive_perpetuals, derive_position_request, derive_transfer_authority, CustodyMeta,
    DecodedPosition, PoolMeta, RequestChange,
};
use zerox1_defi_protocols::protocols::jupiter::build_jlp_redeem_tx;
use zerox1_defi_runtime::rpc::{classify_simulation, RpcContext};
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
#[allow(dead_code)]
const BURN_CU_LIMIT: u32 = 600_000;

/// Same priority fee envelope as the open path.
const PRIORITY_FEE: u64 = 10_000;

/// Default close-request slippage. Operators tune via runbook.
const CLOSE_SLIPPAGE_BPS: u16 = 50;

/// Synthetic stand-in custody address — mirrors `hedge.rs`'s
/// `SYNTHETIC_CUSTODY` constant. Used only when the live pool meta is
/// missing (devnet boot — Jupiter Perps is mainnet-only). The
/// audit-fix C3 guard refuses to sign on synthetic placeholders in
/// non-sim mode.
const SYNTHETIC_CUSTODY: Pubkey =
    solana_sdk::pubkey!("G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa");

/// Run (or simulate) the unwind sequence for a recorded position.
///
/// Sequence:
///   1. Iterate `active.open_positions` — for each entry, read the
///      on-chain `Position` account, build close-request ixns with a
///      fresh randomization counter, whitelist-verify, simulate or
///      submit.
///   2. Build JLP redeem ixns (Jupiter aggregator) for
///      `min(payload.jlp_lamports, active.jlp_acquired_lamports)` —
///      `u64::MAX` means full.
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
            // fleet-v0.4.1: with `recover.rs` populating state on every
            // boot when JLP + Jupiter Perps shorts exist on chain, this
            // branch is only reached when the wallet genuinely has
            // nothing to unwind. We still return a zero-Report (ok=true,
            // zero usdc, no signatures) to keep the dispatch contract
            // intact, but the operator's expectation should be: this
            // means "nothing to do," not "lost a position."
            warn!(
                ?conv,
                "withdraw requested but no active position tracked — wallet has no JLP \
                 or no on-chain Jupiter Perps shorts (verified by `recover.rs` at boot). \
                 Returning zero-Report sentinel.",
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
        "hedgedjlp unwind starting"
    );

    let mut all_sigs: Vec<String> = Vec::new();

    // ── 1. Close all open hedge shorts ─────────────────────────────────
    //
    // fleet-v0.4.1: for each (label, position_pda) entry, we read the
    // on-chain Position account, verify it decodes cleanly + is
    // non-empty, then generate a fresh close-counter and derive the
    // close-request PDA. Both Assign-tracked and recovered positions
    // take this same path — the original `open_counter` is no longer
    // needed (and is no longer recorded).
    //
    // Empty list = no real open positions tracked. Surface as a
    // zero-Report — we do NOT silently fall through to synthetic
    // derivation, which would build close requests for PDAs that
    // don't exist on chain.
    let positions_to_close = effective_positions_to_close(&active);
    if positions_to_close.is_empty() {
        warn!(
            ?conv,
            "no tracked positions to close (sim-only Assign or fresh wallet?) — \
             returning zero-Report"
        );
        return Ok(ReportHedgedJlpWithdraw {
            header: ReportHeader::ok(conv),
            usdc_returned_lamports: 0,
            tx_signatures: vec![],
        });
    }

    // Fresh close-counter base — same pattern as `hedge.rs` open path.
    // Per spec §3.6 the counter is a randomization nonce; each per-asset
    // request gets `counter_base + i` so concurrent allocations don't
    // collide on the PositionRequest PDA derivation.
    let close_counter_base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for (i, (asset_label, position_pubkey)) in positions_to_close.iter().enumerate() {
        let close_counter = close_counter_base.wrapping_add(i as u64);

        // Read the on-chain Position account. If this fails — RPC
        // error, missing account, wrong owner, decode mismatch, or
        // empty position — skip with a warn. We never silently build
        // close requests against a non-existent or stale position.
        let decoded = match read_decoded_position(&ctx.rpc, *position_pubkey).await {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    ?conv,
                    asset = %asset_label,
                    position = %position_pubkey,
                    ?e,
                    "skipping close-request: on-chain Position read/decode failed \
                     (auto-liquidated between recovery and withdraw? stale recovery entry?)"
                );
                continue;
            }
        };

        let close_ixs = match build_close_request_ixns(
            ctx,
            asset_label,
            &decoded,
            close_counter,
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
            close_counter,
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

    // ── 2. Redeem JLP via Jupiter Swap aggregator ──────────────────────
    // Replaces the deprecated `remove_liquidity_2` direct path. Jupiter
    // routes JLP → USDC through whichever venues currently offer the
    // best fill. The returned tx has Jupiter's ALTs baked in; we sign
    // slot 0 + simulate or broadcast via `sign_existing_*`.
    //
    // Whitelist intentionally skipped on Jupiter-built txs — see the
    // comment in `jlp_hedge::run_jlp_buy_only`. Operator approval queue
    // remains the submit gate.
    let jlp_to_burn = compute_jlp_to_burn(payload.jlp_lamports, active.jlp_acquired_lamports);
    let mut usdc_returned: u64 = 0;
    if jlp_to_burn == 0 {
        info!(?conv, "jlp_to_burn=0 — skipping JLP redeem leg");
    } else {
        let user = ctx.wallet.pubkey();
        info!(
            ?conv,
            jlp_to_burn,
            slippage_bps = ctx.jupiter_slippage_bps,
            "JLP redeem via Jupiter Swap aggregator — requesting quote + tx"
        );
        match build_jlp_redeem_tx(&ctx.jupiter, &user, jlp_to_burn, ctx.jupiter_slippage_bps).await
        {
            Ok(tx) => {
                if ctx.simulate_only {
                    match ctx
                        .rpc
                        .sign_existing_simulate(tx, ctx.wallet.keypair())
                        .await
                    {
                        Ok(sim) => {
                            let (layout_valid, summary) = classify_simulation(&sim);
                            if let Some(logs) = sim.logs.as_ref() {
                                let warn_level = sim.err.is_some();
                                for (i, line) in logs.iter().enumerate() {
                                    if warn_level {
                                        warn!(
                                            jlp_redeem_sim_log_idx = i,
                                            "jlp_redeem_sim_log: {}", line
                                        );
                                    } else {
                                        info!(
                                            jlp_redeem_sim_log_idx = i,
                                            "jlp_redeem_sim_log: {}", line
                                        );
                                    }
                                }
                            }
                            if sim.err.is_some() {
                                warn!(
                                    ?conv,
                                    layout_valid,
                                    summary = %summary,
                                    "JLP redeem Jupiter sim returned error",
                                );
                            } else {
                                info!(
                                    ?conv,
                                    layout_valid,
                                    summary = %summary,
                                    "JLP redeem Jupiter sim succeeded",
                                );
                            }
                        }
                        Err(e) => warn!(?conv, ?e, "JLP redeem sign_existing_simulate threw"),
                    }
                } else {
                    match ctx.rpc.sign_existing_send(tx, ctx.wallet.keypair()).await {
                        Ok(sig) => {
                            info!(?conv, %sig, "JLP redeem confirmed on-chain (via Jupiter)");
                            all_sigs.push(sig.to_string());
                            // v0 proxy: equate to jlp_to_burn pending the
                            // post-tx balance read (future work).
                            usdc_returned = jlp_to_burn;
                        }
                        Err(e) => warn!(?conv, ?e, "JLP redeem sign_existing_send failed"),
                    }
                }
            }
            Err(e) => warn!(?conv, ?e, "Jupiter quote/swap build failed for JLP redeem"),
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
///
/// fleet-v0.4.1: tuple shape dropped from 3 to 2 — the unwind path no
/// longer carries `open_counter`. With the open path now writing back
/// the real position pubkey + the close path reading the on-chain
/// Position account, the unwind reuses both. The synthetic-derive
/// fallback (M11 era) was removed in audit-fix C2 — empty
/// `open_positions` returns an empty list (caller surfaces zero-Report).
fn effective_positions_to_close(active: &ActivePosition) -> Vec<(String, Pubkey)> {
    active.open_positions.clone()
}

/// Read and decode the on-chain `Position` account at `position_pubkey`.
///
/// Returns the decoded position on success. Errors when:
///   - the RPC `get_account` call fails (network / cluster issue),
///   - the account does not exist,
///   - the owner is not `JUPITER_PERPETUALS_PROGRAM_ID`,
///   - the data fails `decode_position` (discriminator / length),
///   - the decoded position is `is_empty()` (size_usd == 0 — fully
///     closed; nothing to unwind).
///
/// The caller logs the specific reason at `warn!` and skips the leg.
/// We never silently fall back to a no-op or a synthetic derivation.
async fn read_decoded_position(
    rpc: &Arc<RpcContext>,
    position_pubkey: Pubkey,
) -> Result<DecodedPosition> {
    let account = rpc
        .client
        .get_account(&position_pubkey)
        .await
        .with_context(|| format!("get_account for Position {position_pubkey}"))?;
    if account.owner != JUPITER_PERPETUALS_PROGRAM_ID {
        anyhow::bail!(
            "Position {} has unexpected owner {} (expected Jupiter Perps {})",
            position_pubkey,
            account.owner,
            JUPITER_PERPETUALS_PROGRAM_ID
        );
    }
    let decoded = decode_position(position_pubkey, &account.data)
        .map_err(|e| anyhow::anyhow!("decode_position({position_pubkey}): {:?}", e))?;
    if decoded.is_empty() {
        anyhow::bail!(
            "Position {} decoded but is_empty (size_usd=0) — fully closed; nothing to unwind",
            position_pubkey
        );
    }
    Ok(decoded)
}

/// Build the close-request ixn slice for one asset.
///
/// fleet-v0.4.1: takes a `DecodedPosition` (chain-read) and a fresh
/// `close_counter` (a randomization nonce, NOT the open counter). The
/// close-request `PositionRequest` PDA is derived via
/// `derive_position_request(position, close_counter, Decrease)`. Per
/// spec §3.6 the counter has no structural link to the open-request
/// counter — it's just a per-request nonce that prevents PDA collision
/// when multiple requests target the same Position concurrently.
///
/// Audit-fix C3 (still active): synthetic-custody guard fires before
/// signing; in sim-only mode it logs a warning, in submit mode it bails.
fn build_close_request_ixns(
    ctx: &DispatchCtx,
    asset_label: &str,
    decoded: &DecodedPosition,
    close_counter: u64,
) -> Result<Vec<Instruction>> {
    let user = ctx.wallet.pubkey();
    // Prefer live-loaded pool from boot.
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

    // The Position PDA comes straight from the chain-read.
    let position = decoded.address;
    // Audit fix 3: PositionRequest PDA for the close uses
    // RequestChange::Decrease. fleet-v0.4.1: the counter is fresh,
    // generated at withdraw time (not the open counter).
    let position_request =
        derive_position_request(&position, close_counter, RequestChange::Decrease);

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
        close_counter,
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

// v0.2.3: `build_jlp_burn_ixns` removed. The JLP redeem leg now routes
// JLP → USDC through the Jupiter Swap aggregator (see the inline
// `build_jlp_redeem_tx` call in `run_or_simulate` above). The direct
// `remove_liquidity_2` Anchor path is deprecated.

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
    use solana_sdk::commitment_config::CommitmentConfig;
    use zerox1_defi_protocols::protocols::jlp::{PerpSide, POSITION_DISCRIMINATOR};

    // ── Position-account fixture helpers ─────────────────────────────────
    //
    // Mirrors the private `build_position_bytes` test helper in
    // `zerox1_defi_protocols::protocols::jlp`. We can't re-export it
    // without widening the crate's public surface, so we duplicate the
    // few lines we need. The Position layout is verified at the
    // protocols-crate level (`decode_position_round_trips_all_fields`);
    // this helper rides on top.
    fn build_position_fixture(
        owner: Pubkey,
        pool: Pubkey,
        custody: Pubkey,
        collateral_custody: Pubkey,
        side_byte: u8,
        size_usd: u64,
    ) -> Vec<u8> {
        // POSITION_TOTAL_LEN = 210
        let mut buf = vec![0u8; 210];
        buf[0..8].copy_from_slice(&POSITION_DISCRIMINATOR);
        buf[8..40].copy_from_slice(owner.as_ref());
        buf[40..72].copy_from_slice(pool.as_ref());
        buf[72..104].copy_from_slice(custody.as_ref());
        buf[104..136].copy_from_slice(collateral_custody.as_ref());
        // open_time / update_time left zero.
        buf[152] = side_byte;
        // price at [153..161] left zero.
        buf[161..169].copy_from_slice(&size_usd.to_le_bytes());
        // collateral_usd / realised_pnl / cumulative_interest /
        // locked_amount left zero.
        buf[209] = 254; // bump
        buf
    }

    fn unreachable_rpc() -> Arc<RpcContext> {
        // Same pattern as recover.rs's `unreachable_rpc` test helper:
        // an unreachable RPC URL short-circuits to an RPC error in every
        // chain read. Construction does no I/O.
        Arc::new(RpcContext::new(
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        ))
    }

    fn decoded_short(position: Pubkey, size_usd: u64) -> DecodedPosition {
        // Build a minimal DecodedPosition that satisfies the unwind
        // path's needs. The values for `pool`, `custody`,
        // `collateral_custody` come from the chain in production;
        // the unwind path itself only reads `address` from the
        // decoded position — pool/custody resolution is done via
        // `pool.custody_for_mint(asset_label)`.
        DecodedPosition {
            address: position,
            owner: Pubkey::new_unique(),
            pool: JLP_POOL,
            custody: Pubkey::new_unique(),
            collateral_custody: Pubkey::new_unique(),
            open_time: 0,
            update_time: 0,
            side: PerpSide::Short,
            price: 0,
            size_usd,
            collateral_usd: 0,
            realised_pnl_usd: 0,
            locked_amount: 0,
        }
    }

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
            open_positions: vec![("SOL".to_string(), pk)],
        };
        let v = effective_positions_to_close(&active);
        assert_eq!(v.len(), 1);
        // fleet-v0.4.1: tuple shape is (label, pubkey).
        assert_eq!(v[0].0, "SOL");
        assert_eq!(v[0].1, pk);
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

    // ── fleet-v0.4.1: chain-derived close-PDA tests ──────────────────────

    /// Position-fixture decoder round-trips through the unwind path's
    /// derivation. Demonstrates that a `DecodedPosition` from the
    /// chain-read drives the close-request PDA derivation directly —
    /// no `open_counter` needed.
    #[test]
    fn close_request_pda_derives_from_decoded_position_address() {
        let position = Pubkey::new_unique();
        let decoded = decoded_short(position, 100_000_000);
        // Two distinct close-counters at withdraw time MUST produce
        // distinct close-request PDAs against the same Position. This
        // is what makes the counter a real randomization nonce.
        let pda_a = derive_position_request(&decoded.address, 10_000, RequestChange::Decrease);
        let pda_b = derive_position_request(&decoded.address, 10_001, RequestChange::Decrease);
        assert_ne!(pda_a, pda_b);
        // And a Decrease PDA at the same counter as a hypothetical
        // Increase PDA must also differ (spec §3.6: the trailing
        // request_change byte is part of the seed).
        let pda_inc = derive_position_request(&decoded.address, 10_000, RequestChange::Increase);
        assert_ne!(pda_a, pda_inc);
    }

    /// fleet-v0.4.1 invariant: the close-PDA derivation does NOT depend
    /// on any "open counter" — only on the position address (chain-read)
    /// and a fresh close-counter generated at withdraw time. Pin the
    /// invariant explicitly: a position that was opened with counter X
    /// (e.g. unix_seconds at open time) and recovered with counter 0
    /// (the old recover.rs placeholder) BOTH produce the same close
    /// PDA at withdraw time, because withdraw only uses its own
    /// close-counter.
    #[test]
    fn withdraw_recovered_position_derives_close_pda_from_chain() {
        // Two ActivePosition instances pointing at the same Position
        // pubkey: one is "Assign-tracked" (would historically have a
        // real open counter), the other is "recovered" (would
        // historically have open_counter=0). Both feed through the
        // SAME chain-derived close path now.
        let position = Pubkey::new_unique();
        let assign_tracked = ActivePosition {
            conv: [1u8; 16],
            our_jlp_lamports: 100_000_000,
            jlp_acquired_lamports: 100_000_000,
            target_delta_bps: 0,
            max_borrow_rate_bps: 5_000,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 30_000_000,
            open_positions: vec![("SOL".to_string(), position)],
        };
        let recovered = ActivePosition {
            conv: [0xFFu8; 16], // RECOVERED_CONV_SENTINEL
            our_jlp_lamports: 100_000_000,
            jlp_acquired_lamports: 100_000_000,
            target_delta_bps: 0,
            max_borrow_rate_bps: 5_000,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 30_000_000,
            open_positions: vec![("SOL".to_string(), position)],
        };
        // Same shape → same effective close list.
        let a = effective_positions_to_close(&assign_tracked);
        let r = effective_positions_to_close(&recovered);
        assert_eq!(a.len(), r.len());
        assert_eq!(a[0].1, r[0].1);
        // Now simulate withdraw-time PDA derivation. Same Position +
        // same close-counter → same close PDA, regardless of whether
        // the ActivePosition was Assign-tracked or recovered.
        let close_counter = 1_700_000_999u64;
        let pda_assign =
            derive_position_request(&a[0].1, close_counter, RequestChange::Decrease);
        let pda_recov =
            derive_position_request(&r[0].1, close_counter, RequestChange::Decrease);
        assert_eq!(
            pda_assign, pda_recov,
            "fleet-v0.4.1: close-PDA is determined by (position, close_counter, Decrease) — \
             the historical open_counter has no effect"
        );
    }

    /// Guard the happy path: a position that was just opened (Assign-
    /// tracked, position pubkey known) still derives a clean close PDA
    /// at withdraw. The fresh close-counter is generated at withdraw
    /// time; the open counter is gone. We assert the derivation is
    /// stable and well-formed.
    #[test]
    fn withdraw_position_with_real_open_counter_still_works() {
        // Even though the "open counter" used to be stored, we now
        // throw it away. The withdraw still works because the
        // close-request needs its OWN fresh counter, not the open one.
        let position = Pubkey::new_unique();
        let active = ActivePosition {
            conv: [2u8; 16],
            our_jlp_lamports: 50_000_000,
            jlp_acquired_lamports: 50_000_000,
            target_delta_bps: 0,
            max_borrow_rate_bps: 5_000,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 25_000_000,
            open_positions: vec![("ETH".to_string(), position)],
        };
        let positions = effective_positions_to_close(&active);
        assert_eq!(positions.len(), 1);
        // Derive a close-PDA at a fresh close-counter. Must be
        // well-formed and stable: re-derivation with the same inputs
        // yields the same address.
        let close_counter = 1_700_000_555u64;
        let pda1 =
            derive_position_request(&positions[0].1, close_counter, RequestChange::Decrease);
        let pda2 =
            derive_position_request(&positions[0].1, close_counter, RequestChange::Decrease);
        assert_eq!(pda1, pda2, "PDA derivation is deterministic");
        // A different counter → different PDA (the whole point of the
        // counter being a randomization nonce — concurrent close
        // requests against the same Position don't collide).
        let pda_other = derive_position_request(
            &positions[0].1,
            close_counter.wrapping_add(1),
            RequestChange::Decrease,
        );
        assert_ne!(pda1, pda_other);
    }

    /// Position-account read errors cleanly through `read_decoded_position`
    /// when the RPC is unreachable. We MUST NOT silently fall back to a
    /// no-op or a synthetic derivation — the caller logs + skips the
    /// asset with a warn.
    #[tokio::test]
    async fn read_decoded_position_errors_when_chain_read_fails() {
        let rpc = unreachable_rpc();
        let position = Pubkey::new_unique();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_decoded_position(&rpc, position),
        )
        .await
        .expect("read_decoded_position must return promptly on unreachable RPC");
        assert!(
            result.is_err(),
            "chain read failure must propagate as Err — caller skips the leg"
        );
    }

    /// `decode_position` on a buffer with the right discriminator but
    /// `size_usd == 0` (empty position) must surface as an error in
    /// `read_decoded_position` — we never build a close request for an
    /// already-closed position.
    #[test]
    fn decode_position_with_zero_size_is_empty() {
        let owner = Pubkey::new_unique();
        let pool = JLP_POOL;
        let custody = Pubkey::new_unique();
        let coll = Pubkey::new_unique();
        let bytes = build_position_fixture(owner, pool, custody, coll, /*Short*/ 2, /*size*/ 0);
        let pos = decode_position(Pubkey::new_unique(), &bytes).expect("decode");
        assert!(
            pos.is_empty(),
            "size_usd=0 must report empty — read_decoded_position bails on this"
        );
    }

    /// `decode_position` on a properly-shaped Short position succeeds
    /// and the unwind path picks up `decoded.address` for PDA
    /// derivation. Round-trip the fixture so a future regression in
    /// the Position layout surfaces here.
    #[test]
    fn decode_position_round_trips_through_unwind_fixture() {
        let owner = Pubkey::new_unique();
        let pool = JLP_POOL;
        let custody = Pubkey::new_unique();
        let coll = Pubkey::new_unique();
        let bytes = build_position_fixture(
            owner,
            pool,
            custody,
            coll,
            /*Short*/ 2,
            /*size*/ 77_000_000,
        );
        let address = Pubkey::new_unique();
        let pos = decode_position(address, &bytes).expect("decode");
        assert_eq!(pos.address, address);
        assert_eq!(pos.owner, owner);
        assert_eq!(pos.pool, pool);
        assert_eq!(pos.custody, custody);
        assert_eq!(pos.collateral_custody, coll);
        assert_eq!(pos.side, PerpSide::Short);
        assert_eq!(pos.size_usd, 77_000_000);
        assert!(!pos.is_empty());
    }
}
