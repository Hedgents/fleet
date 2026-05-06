//! Leverage loop for multiply-daemon (M6).
//!
//! Multi-round supply→borrow→stake→supply walk to a target LTV. Each
//! iteration is one Solana transaction containing:
//!
//!   1. kamino borrow SOL    → user wSOL ATA          (3 ixs)
//!   2. spl-token CloseAccount on wSOL ATA → SOL flows to user wallet
//!   3. jito DepositSol      → user jitoSOL ATA       (2 ixs)
//!   4. kamino refresh + deposit jitoSOL collateral   (2 ixs)
//!
//! Total ~8 ixs per round, fits in a v0 transaction without ALTs. The body
//! of one iteration is lifted near-verbatim from the monolith's
//! `defi-daemon/src/handlers/multiply.rs::lever_up`. The new logic in M6 is
//! the **multi-round walk**: repeatedly query LTV and lever up until either
//! the target is hit (within 50 bps), the round cap fires, or the deadline
//! lapses.
//!
//! Sim-only mode (`ctx.simulate_only`): every iteration is simulated via
//! `RpcContext::build_sign_simulate`. No tx is broadcast and on-chain LTV
//! does not advance, so the loop exits after one simulated round (it would
//! otherwise spin to MAX_LEVERAGE_LOOP_ROUNDS with the same starting LTV).

use anyhow::{anyhow, bail, Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use spl_token::instruction::close_account;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use zerox1_defi_protocols::{
    constants::{
        JITOSOL_MINT, KAMINO_MAIN_JITOSOL_RESERVE, KAMINO_MAIN_MARKET,
        KAMINO_MAIN_SOL_RESERVE, TOKEN_PROGRAM_ID, WSOL_MINT,
    },
    protocols::{
        jito::deposit_sol_ix,
        jito_loader::load_jito_pool,
        kamino::{
            borrow_obligation_liquidity_ix, deposit_collateral_only_ix, refresh_reserve_ix,
            ReserveAccounts,
        },
        kamino_loader::{load_reserve, query_position_ltv_bps},
    },
    util::ata,
};
use zerox1_protocol::fleet::multiply::{AssignMultiply, ReportMultiply};
use zerox1_protocol::fleet::ReportHeader;

use crate::caps;
use crate::dispatch::DispatchCtx;

/// Compute budget per leverage iteration. ~700k CU is enough for 8 ixs
/// (deposit ~200k + jito DepositSol ~150k + borrow ~150k + overhead).
const MULTIPLY_CU_LIMIT: u32 = 800_000;
const MULTIPLY_PRIORITY_FEE: u64 = 10_000;

/// Stop the round loop when within this many bps of target — borrowing the
/// last few bps risks oscillation and small-quantity ix failures.
const TARGET_PROXIMITY_BPS: u16 = 50;

/// Either simulate the leverage entry or actually submit it (per
/// `ctx.simulate_only`).
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    assign: &AssignMultiply,
    conv: [u8; 16],
) -> Result<ReportMultiply> {
    let user = ctx.wallet.pubkey();
    let lending_market = KAMINO_MAIN_MARKET;

    // Deadline check up front — fail fast rather than enter a loop we
    // know we cannot finish.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if assign.deadline_unix > 0 && assign.deadline_unix < now {
        bail!(
            "AssignMultiply deadline {} has passed (now {})",
            assign.deadline_unix,
            now
        );
    }

    info!(
        simulate_only = ctx.simulate_only,
        target_ltv_bps = assign.target_ltv_bps,
        max_slippage_bps = assign.max_slippage_bps,
        "leverage loop starting"
    );

    // Read current LTV.
    let mut current_ltv = query_position_ltv_bps(&ctx.rpc.client, user, lending_market)
        .await
        .context("query initial LTV")?;
    info!(
        current_ltv_bps = current_ltv,
        target_ltv_bps = assign.target_ltv_bps,
        "leverage loop entering"
    );

    if current_ltv >= assign.target_ltv_bps {
        info!("already at or above target; no work to do");
        return Ok(ReportMultiply {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: current_ltv,
            tx_signature: None,
        });
    }

    // Pre-load reserves + jito pool. These don't change between rounds.
    let sol_reserve = load_reserve(&ctx.rpc.client, &KAMINO_MAIN_SOL_RESERVE, WSOL_MINT, &lending_market)
        .await
        .context("load SOL reserve")?;
    let jitosol_reserve =
        load_reserve(&ctx.rpc.client, &KAMINO_MAIN_JITOSOL_RESERVE, JITOSOL_MINT, &lending_market)
            .await
            .context("load jitoSOL reserve")?;
    let jito_pool = load_jito_pool(&ctx.rpc.client)
        .await
        .context("load Jito stake pool")?;

    let mut last_signature: Option<String> = None;

    for round in 1..=caps::MAX_LEVERAGE_LOOP_ROUNDS {
        let headroom_bps = assign.target_ltv_bps.saturating_sub(current_ltv);
        if headroom_bps < TARGET_PROXIMITY_BPS {
            info!(
                round,
                current_ltv_bps = current_ltv,
                "within {TARGET_PROXIMITY_BPS} bps of target — stopping"
            );
            break;
        }

        // Per-iteration deadline check.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if assign.deadline_unix > 0 && assign.deadline_unix < now {
            warn!(
                round,
                current_ltv_bps = current_ltv,
                deadline = assign.deadline_unix,
                "deadline reached during loop; reporting partial progress"
            );
            break;
        }

        let rounds_left = (caps::MAX_LEVERAGE_LOOP_ROUNDS - round + 1) as u64;
        // Naive spread of the operating budget across remaining rounds.
        // M9 will replace this with an LTV-driven sizing function.
        let per_round_borrow_lamports = ctx
            .args_max_position_usdc_lamports
            .saturating_div(rounds_left);
        if per_round_borrow_lamports == 0 {
            warn!(round, "computed per-round borrow is zero; nothing to do");
            break;
        }

        // jitoSOL deposited per round: assume 1 SOL ≈ 1 jitoSOL (Jito's
        // exchange rate moves slowly — 0.5% safety margin).
        let expected_jitosol_received =
            per_round_borrow_lamports.saturating_sub(per_round_borrow_lamports / 200);

        info!(
            round,
            borrow_lamports = per_round_borrow_lamports,
            expected_jitosol = expected_jitosol_received,
            current_ltv_bps = current_ltv,
            "lever-up round"
        );

        let outcome = run_one_lever_up_iteration(
            ctx,
            user,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            per_round_borrow_lamports,
            expected_jitosol_received,
        )
        .await
        .with_context(|| format!("round {round} lever-up"))?;

        if let Some(sig) = outcome.tx_signature {
            last_signature = Some(sig);
        }

        if ctx.simulate_only {
            // Simulation does not move on-chain state. Re-querying LTV
            // would yield the same value forever; one simulated round is
            // enough to prove the iteration shape is valid.
            info!(
                round,
                "simulate-only: stopping after one iteration (chain state unchanged)"
            );
            break;
        }

        // Submit-mode: re-read LTV from chain and continue.
        current_ltv = query_position_ltv_bps(&ctx.rpc.client, user, lending_market)
            .await
            .context("re-query LTV after round")?;
        info!(round, current_ltv_bps = current_ltv, "round committed");

        if round == caps::MAX_LEVERAGE_LOOP_ROUNDS && current_ltv < assign.target_ltv_bps {
            warn!(
                rounds = caps::MAX_LEVERAGE_LOOP_ROUNDS,
                final_ltv_bps = current_ltv,
                target_ltv_bps = assign.target_ltv_bps,
                "max rounds reached, target not hit"
            );
        }
    }

    Ok(ReportMultiply {
        header: ReportHeader::ok(conv),
        resulting_ltv_bps: current_ltv,
        tx_signature: last_signature,
    })
}

struct IterationOutcome {
    tx_signature: Option<String>,
}

/// Build + sim/submit one leverage iteration. Lifted from the monolith's
/// `lever_up` handler body (defi-daemon/src/handlers/multiply.rs) — minus
/// the axum extractors / JSON shape / error helpers.
async fn run_one_lever_up_iteration(
    ctx: &DispatchCtx,
    user: Pubkey,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
    jito_pool: &zerox1_defi_protocols::protocols::jito::StakePoolMeta,
    borrow_sol_amount: u64,
    expected_jitosol_received: u64,
) -> Result<IterationOutcome> {
    if borrow_sol_amount == 0 {
        return Err(anyhow!("borrow_sol_amount must be > 0"));
    }
    if expected_jitosol_received == 0 {
        return Err(anyhow!("expected_jitosol_received must be > 0"));
    }

    let user_wsol_ata = ata(&user, &WSOL_MINT);

    // Step 1: borrow SOL → user wSOL ATA  (ATA-create + refresh + borrow).
    let mut ixs: Vec<Instruction> =
        borrow_obligation_liquidity_ix(&user, sol_reserve, borrow_sol_amount)
            .context("build borrow_obligation_liquidity_ix")?;

    // Step 2: close wSOL ATA so lamports flow to the user wallet (Jito
    // DepositSol takes raw SOL, not wSOL).
    let close_wsol = close_account(
        &TOKEN_PROGRAM_ID,
        &user_wsol_ata,
        &user, // destination
        &user, // authority
        &[],   // no multisig
    )
    .context("build spl-token close_account")?;
    ixs.push(close_wsol);

    // Step 3: jito DepositSol → user jitoSOL ATA.
    let jito_ixs = deposit_sol_ix(&user, jito_pool, borrow_sol_amount)
        .context("build deposit_sol_ix")?;
    ixs.extend(jito_ixs);

    // Step 4: refresh jitoSOL reserve + deposit collateral.
    ixs.push(refresh_reserve_ix(jitosol_reserve));
    let deposit_collateral =
        deposit_collateral_only_ix(&user, jitosol_reserve, expected_jitosol_received)
            .context("build deposit_collateral_only_ix")?;
    ixs.push(deposit_collateral);

    let ix_count = ixs.len();

    if ctx.simulate_only {
        let sim = ctx
            .rpc
            .build_sign_simulate(
                ixs,
                ctx.wallet.keypair(),
                MULTIPLY_CU_LIMIT,
                MULTIPLY_PRIORITY_FEE,
            )
            .await
            .context("simulate leverage tx")?;
        let (layout_valid, summary) =
            zerox1_defi_runtime::rpc::classify_simulation(&sim);
        info!(
            ix_count,
            layout_valid,
            summary = %summary,
            "round sim ok"
        );
        Ok(IterationOutcome {
            tx_signature: None,
        })
    } else {
        let sig = ctx
            .rpc
            .build_sign_send(
                ixs,
                ctx.wallet.keypair(),
                MULTIPLY_CU_LIMIT,
                MULTIPLY_PRIORITY_FEE,
            )
            .await
            .context("broadcast leverage tx")?;
        info!(ix_count, sig = %sig, "round committed");
        Ok(IterationOutcome {
            tx_signature: Some(sig.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_proximity_bps_sane() {
        assert!(TARGET_PROXIMITY_BPS > 0);
        assert!(TARGET_PROXIMITY_BPS < caps::MAX_LTV_BPS);
    }

    #[test]
    fn cu_limit_sane() {
        // klend deposit + borrow + jito DepositSol + refresh fits well under 1.4M.
        assert!(MULTIPLY_CU_LIMIT > 400_000);
        assert!(MULTIPLY_CU_LIMIT < 1_400_000);
    }
}
