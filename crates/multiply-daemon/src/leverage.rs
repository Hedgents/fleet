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
//! Total ~14 ixs per round; from v0.1.20 the bundle is compiled with
//! Kamino's main-market Address Lookup Table to stay under the 1232-byte
//! raw tx limit (live without ALTs: 1648 bytes, 4 over). The body
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
        KAMINO_MAIN_MARKET_LOOKUP_TABLE, KAMINO_MAIN_SOL_RESERVE, TOKEN_PROGRAM_ID, WSOL_MINT,
    },
    protocols::{
        jito::deposit_sol_ix,
        jito_loader::load_jito_pool,
        kamino::{
            borrow_obligation_liquidity_ix, borrow_obligation_liquidity_v2_ix,
            deposit_reserve_liquidity_and_obligation_collateral_v2_ix,
            derive_user_obligation_with_seed, refresh_obligation_ix, refresh_reserve_ix,
            ReserveAccounts,
        },
        kamino_loader::{fetch_obligation, load_reserve, query_position_ltv_bps_with_seed},
    },
    util::ata,
};
use zerox1_protocol::fleet::multiply::{AssignMultiply, ReportMultiply};
use zerox1_protocol::fleet::ReportHeader;

use crate::caps;
use crate::dispatch::DispatchCtx;

/// Compute budget per leverage iteration. v0.1.17 bumps to 1_000_000 to
/// cover the v2-handler farm CPI (CollateralFarm refresh on jitoSOL deposit
/// + DebtFarm refresh on SOL borrow). The CPI adds ~100-150k CU per side.
const MULTIPLY_CU_LIMIT: u32 = 1_000_000;
const MULTIPLY_PRIORITY_FEE: u64 = 10_000;

/// Stop the round loop when within this many bps of target — borrowing the
/// last few bps risks oscillation and small-quantity ix failures.
const TARGET_PROXIMITY_BPS: u16 = 50;

/// rc36 + rc38 + rc39 borrow clamp. Returns the per-round SOL borrow
/// amount in lamports, clamped to whatever bf-adjusted headroom remains
/// on the obligation after pessimistically adding `in_flight_bf_debt_sf`
/// to the reported `bf_debt_value_sf`. Pure math so the multi-round
/// propagation is unit-testable without a chain mock. A 90% safety
/// factor applies on top to cover BF drift between value read and tx
/// landing plus small slippage overhead.
///
/// rc39: `borrow_factor_bps` is the reserve's BF in basis points
/// (10_000 = no adjustment, 12_500 = 1.25× — SOL on Kamino main).
/// Kamino's `BorrowObligationLiquidityV2` check uses BF-adjusted USD
/// for both `allowed_borrow_value` and the new debt being added. Since
/// `sol_value_per_lamport_sf` is the raw market-value/lamport ratio,
/// we have to multiply it by `borrow_factor_bps / 10_000` before
/// dividing `safe_headroom_sf` by it — otherwise the clamp under-
/// divides and asks ~`borrow_factor_bps/10000 - 1` more than the chain
/// will accept (live: 25% overshoot → Anchor 6013 `BorrowTooLarge`).
///
/// Returns `naive_per_round` (uncapped) when `sol_value_per_lamport_sf`
/// is zero (no prior SOL borrow on the obligation → no price ratio to
/// convert headroom-USD into lamports). That matches pre-rc36 behaviour
/// for the first round on a fresh wallet; Kamino's on-chain check is
/// still the safety net.
pub(crate) fn clamp_borrow_to_headroom(
    naive_per_round: u64,
    allowed_borrow_value_sf: u128,
    bf_debt_value_sf: u128,
    in_flight_bf_debt_sf: u128,
    sol_value_per_lamport_sf: u128,
    borrow_factor_bps: u32,
) -> u64 {
    if sol_value_per_lamport_sf == 0 {
        return naive_per_round;
    }
    let effective_bf_debt_sf = bf_debt_value_sf.saturating_add(in_flight_bf_debt_sf);
    let headroom_sf = allowed_borrow_value_sf.saturating_sub(effective_bf_debt_sf);
    let safe_headroom_sf = headroom_sf.saturating_mul(9).saturating_div(10);
    // bf_adjusted price-per-lamport: each borrowed lamport occupies
    // `price × BF` of the BF-adjusted allowed_borrow budget.
    let bf_adjusted_price_sf = sol_value_per_lamport_sf
        .saturating_mul(borrow_factor_bps as u128)
        .saturating_div(10_000);
    let safe_lamports = safe_headroom_sf.saturating_div(bf_adjusted_price_sf.max(1));
    let safe_lamports_u64 = safe_lamports.min(u64::MAX as u128) as u64;
    naive_per_round.min(safe_lamports_u64)
}

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

    // v0.1.11 Bug 1 fix: seed the obligation with an initial jitoSOL
    // deposit before the leverage loop's first borrow. On a fresh wallet,
    // round 1's borrow_obligation_liquidity_ix would fail with Custom(6051)
    // (zero collateral). The seed is a no-op when the obligation already
    // holds collateral. Sim-only mode simulates the seed bundle but does
    // not broadcast.
    // rc47: when the orchestrator routed USDC into this Assign,
    // `dispatch::handle_assign` already swapped USDC → native SOL into the
    // wallet via `seed::seed_with_usdc`. We must then stake that SOL into
    // the obligation as additional jitoSOL collateral — even when the
    // obligation already holds jitoSOL — or the new principal contribution
    // gets stranded as wallet SOL.
    let force_top_up = assign.usdc_lamports > 0;
    let seeded = crate::seed::maybe_seed_obligation(ctx, force_top_up)
        .await
        .context("maybe_seed_obligation")?;
    if seeded && ctx.simulate_only {
        // Simulation does not move on-chain state, so re-querying LTV
        // would still be 0 and round 1 would still hit Custom(6051) in
        // the simulator. Return early with a sim-only report — the
        // operator's logs show the seed bundle simulated cleanly.
        info!("simulate-only: seed simulated; skipping leverage round simulation");
        return Ok(ReportMultiply {
            header: ReportHeader::ok(conv),
            resulting_ltv_bps: 0,
            tx_signature: None,
        });
    }

    // Read current LTV. rc38: must use the seeded obligation
    // PDA — `query_position_ltv_bps` without a seed silently reads the
    // (0,0) stable-yield obligation, returning 0 for a multiply position
    // and breaking the loop's halt condition.
    let mut current_ltv = query_position_ltv_bps_with_seed(
        &ctx.rpc.client,
        user,
        lending_market,
        caps::MULTIPLY_OBLIGATION_SEED.0,
        caps::MULTIPLY_OBLIGATION_SEED.1,
    )
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
    let sol_reserve = load_reserve(
        &ctx.rpc.client,
        &KAMINO_MAIN_SOL_RESERVE,
        WSOL_MINT,
        &lending_market,
    )
    .await
    .context("load SOL reserve")?;
    let jitosol_reserve = load_reserve(
        &ctx.rpc.client,
        &KAMINO_MAIN_JITOSOL_RESERVE,
        JITOSOL_MINT,
        &lending_market,
    )
    .await
    .context("load jitoSOL reserve")?;
    let jito_pool = load_jito_pool(&ctx.rpc.client)
        .await
        .context("load Jito stake pool")?;

    let mut last_signature: Option<String> = None;

    // v0.1.21 fix: fetch the obligation's currently-registered reserves ONCE
    // at the start of the loop, then locally bump the in-memory model after
    // each successful round. Previously we re-fetched per round, which raced
    // against RPC read-replica lag — the replica we hit between rounds had
    // sometimes not yet ingested the previous round's slot, returning the
    // pre-borrow obligation state and dropping the SOL borrow reserve from
    // RefreshObligation's remaining_accounts (→ InvalidAccountInput on the
    // next round's first RefreshObligation).
    //
    // The state transitions are deterministic: every successful round adds
    // a SOL borrow (if not already present) and a jitoSOL deposit (always
    // present post-seed). So we can track them locally.
    let obligation_addr = derive_user_obligation_with_seed(
        &user,
        &lending_market,
        caps::MULTIPLY_OBLIGATION_SEED.0,
        caps::MULTIPLY_OBLIGATION_SEED.1,
    );
    let decoded = fetch_obligation(&ctx.rpc.client, &obligation_addr)
        .await
        .context("fetch obligation (initial)")?;
    let mut obligation_reserves: Vec<Pubkey> = decoded
        .as_ref()
        .map(|d| {
            d.deposits
                .iter()
                .map(|x| x.reserve)
                .chain(d.borrows.iter().map(|x| x.reserve))
                .collect()
        })
        .unwrap_or_default();

    // rc36 (2026-05-27): LTV-aware borrow clamp. Pre-rc36, every round
    // computed `per_round_borrow_lamports = max_position_cap /
    // rounds_left` without consulting the actual on-chain borrow
    // capacity. Round N would happily ask Kamino to borrow more than
    // the obligation's `allowed_borrow_value_sf` permitted; Kamino
    // would then reject with `BorrowTooLarge (Error 6013)` and the
    // entire leverage loop would abort mid-deploy.
    //
    // The fix: track Kamino's borrow-capacity numbers across rounds
    // and clamp `per_round_borrow_lamports` to the safe headroom.
    // Bootstrapping a SOL-price ratio from any existing SOL borrow
    // (sf-scaled USD value per lamport) lets us convert headroom_sf
    // → SOL lamports without an external price feed. First round on
    // a fresh wallet has no prior borrow → no clamp → falls back to
    // the naive amount (still safer than pre-rc36 because round-2
    // onward gets clamped once round-1 deposits and a borrow lands).
    let mut allowed_borrow_value_sf = decoded
        .as_ref()
        .map(|d| d.allowed_borrow_value_sf)
        .unwrap_or(0);
    let mut bf_debt_value_sf = decoded
        .as_ref()
        .map(|d| d.borrow_factor_adjusted_debt_value_sf)
        .unwrap_or(0);
    let mut sol_value_per_lamport_sf: Option<u128> = decoded.as_ref().and_then(|d| {
        d.borrows
            .iter()
            .find(|b| b.reserve == KAMINO_MAIN_SOL_RESERVE && b.borrowed_amount_sf > 0)
            .map(|b| {
                // value_sf per lamport, sf-scaled. Divide value_sf
                // (already sf-scaled USD) by borrowed_amount_sf
                // (sf-scaled lamports) yields USD-per-lamport with
                // the same sf-scale.
                b.market_value_sf
                    .saturating_mul(1u128 << 60)
                    .saturating_div(b.borrowed_amount_sf.max(1))
            })
    });

    // rc38: track cumulative borrow that we've broadcast but the
    // re-fetched obligation hasn't yet reflected. Solana RPC read-replica
    // lag can return pre-tx state for hundreds of ms after broadcast even
    // with `confirmed` commitment — round-2 sizing computed only ~50ms
    // after round-1 broadcast saw bf_debt_value_sf unchanged from round-0
    // and overshot by 7% on the live 2026-05-28 test. We pessimistically
    // add this in-flight value (already borrow-factor-adjusted) to the
    // re-fetched `bf_debt_value_sf` so the next round's headroom shrinks
    // even if RPC reads are stale. The counter resets when the chain
    // catches up (see post-round refresh below).
    let mut in_flight_bf_debt_sf: u128 = 0;

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
        let naive_per_round = ctx
            .args_max_position_usdc_lamports
            .saturating_div(rounds_left);

        // rc36: clamp the naive amount to Kamino's actual borrow
        // headroom. `allowed_borrow_value_sf - bf_debt_value_sf` is
        // exactly what Kamino's `BorrowObligationLiquidity` ixn checks
        // server-side; pre-empting it here lets us abort cleanly instead
        // of having a tx fail mid-loop. 90% safety factor covers BF
        // drift between value read and tx landing, plus small slippage
        // overhead.
        let per_round_borrow_lamports = clamp_borrow_to_headroom(
            naive_per_round,
            allowed_borrow_value_sf,
            bf_debt_value_sf,
            in_flight_bf_debt_sf,
            sol_value_per_lamport_sf.unwrap_or(0),
            caps::SOL_BORROW_FACTOR_BPS,
        );
        if per_round_borrow_lamports < naive_per_round {
            info!(
                round,
                naive_per_round,
                clamped = per_round_borrow_lamports,
                in_flight_bf_debt_sf_hi64 = (in_flight_bf_debt_sf >> 64) as u64,
                "rc36+rc38+rc39: per-round borrow clamped to LTV headroom"
            );
        }

        if per_round_borrow_lamports == 0 {
            info!(
                round,
                "rc36: borrow headroom exhausted at current LTV — stopping leverage loop"
            );
            break;
        }

        // v0.1.13 fix: convert SOL → jitoSOL via the pool's on-chain rate
        // (1 jitoSOL ≈ 1.28 SOL on mainnet), then apply a 0.5% safety
        // haircut for pool fees + rounding. The previous 1:1 assumption
        // overstated the jitoSOL output by ~27% and caused the seed
        // bundle's Kamino deposit step to fail with TokenError::InsufficientFunds.
        let rate_adjusted_jitosol = jito_pool.sol_to_jitosol_lamports(per_round_borrow_lamports);
        let expected_jitosol_received =
            rate_adjusted_jitosol.saturating_sub(rate_adjusted_jitosol / 200);

        info!(
            round,
            borrow_lamports = per_round_borrow_lamports,
            expected_jitosol = expected_jitosol_received,
            current_ltv_bps = current_ltv,
            "lever-up round"
        );

        // v0.1.21: obligation_reserves is the locally-tracked model (seeded
        // by the one-shot pre-loop fetch, then bumped after each round's
        // successful broadcast — see below). klend's check_refresh requires
        // a RefreshObligation immediately before BOTH BorrowObligationLiquidity
        // AND DepositObligationCollateral, with `remaining_accounts` matching
        // the obligation's deposit/borrow slots in slot order. After the v0.1.13
        // seed deposit, that's `[jitoSOL reserve]`; after round 1's borrow,
        // klend will also have registered the SOL reserve as a borrow.

        // v0.1.16 fix: klend's RefreshObligation requires every reserve
        // referenced in the obligation (deposits + borrows) to have been
        // refreshed via RefreshReserve earlier in the same transaction.
        // Resolve each obligation-registered Pubkey to its loaded
        // ReserveAccounts so build_lever_up_ixns can emit a RefreshReserve
        // for each one. Currently the multiply obligation can only ever
        // hold the jitoSOL deposit and (after round 1) the SOL borrow —
        // both already loaded above.
        let obligation_reserve_accounts: Vec<&ReserveAccounts> = obligation_reserves
            .iter()
            .map(|res| {
                if *res == sol_reserve.reserve {
                    Ok(&sol_reserve)
                } else if *res == jitosol_reserve.reserve {
                    Ok(&jitosol_reserve)
                } else {
                    Err(anyhow!(
                        "obligation references unknown reserve {res}; \
                         multiply obligation should only hold jitoSOL + SOL"
                    ))
                }
            })
            .collect::<Result<Vec<_>>>()?;

        let outcome = run_one_lever_up_iteration(
            ctx,
            user,
            lending_market,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            per_round_borrow_lamports,
            expected_jitosol_received,
            &obligation_reserve_accounts,
        )
        .await
        .with_context(|| format!("round {round} lever-up"))?;

        if let Some(sig) = outcome.tx_signature {
            last_signature = Some(sig);
        }

        // rc38: add this round's borrow to in-flight bf-debt so the next
        // round's clamp can compensate if the re-fetched obligation reads
        // pre-broadcast state. SOL borrow_factor on Kamino main market is
        // 1.25 (confirmed live: $5.3999 raw → $6.7499 bf-adjusted). If
        // that ever changes we'd want to source it from sol_reserve config
        // rather than hardcoding 125/100, but at the precisions involved
        // the static factor is dominated by the existing 0.9 safety
        // margin on the headroom calc.
        if let Some(price_sf) = sol_value_per_lamport_sf {
            let added_market_value_sf =
                (per_round_borrow_lamports as u128).saturating_mul(price_sf);
            let added_bf_value_sf = added_market_value_sf
                .saturating_mul(125)
                .saturating_div(100);
            in_flight_bf_debt_sf = in_flight_bf_debt_sf.saturating_add(added_bf_value_sf);
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

        // v0.1.21: bump in-memory obligation reserves locally. The round
        // just broadcasted committed a SOL borrow + jitoSOL collateral
        // deposit; the obligation's reserve set is now a superset of what
        // we passed in. We re-derive deterministically rather than rely on
        // re-fetching the obligation account (which races RPC read-replica
        // lag and was the v0.1.20 round-2 failure mode).
        if !obligation_reserves.contains(&jitosol_reserve.reserve) {
            obligation_reserves.push(jitosol_reserve.reserve);
        }
        if !obligation_reserves.contains(&sol_reserve.reserve) {
            obligation_reserves.push(sol_reserve.reserve);
        }

        // Submit-mode: re-read LTV from chain and continue. rc38:
        // seeded variant — see initial-LTV call comment above.
        current_ltv = query_position_ltv_bps_with_seed(
            &ctx.rpc.client,
            user,
            lending_market,
            caps::MULTIPLY_OBLIGATION_SEED.0,
            caps::MULTIPLY_OBLIGATION_SEED.1,
        )
        .await
        .context("re-query LTV after round")?;
        info!(round, current_ltv_bps = current_ltv, "round committed");

        // rc36: re-fetch full obligation state so the next round's
        // borrow clamp uses fresh `allowed_borrow_value_sf` and
        // `borrow_factor_adjusted_debt_value_sf`. Same RPC read-replica
        // lag risk as the rc-v0.1.21 deletion documented, but for
        // value-typed sf fields (not the reserve set) stale reads just
        // mean we under-borrow this round — safer than over-borrowing.
        if let Ok(Some(d)) = fetch_obligation(&ctx.rpc.client, &obligation_addr).await {
            let new_bf_debt = d.borrow_factor_adjusted_debt_value_sf;
            // rc38: if the re-fetched bf_debt has grown since the prior
            // value, the chain has reflected (at least some of) our
            // broadcasted borrows — credit that observed growth back
            // against the in-flight counter. Saturating sub keeps us
            // pessimistic even if the chain somehow grew by more than
            // we're tracking (e.g. concurrent operator action).
            let observed_growth = new_bf_debt.saturating_sub(bf_debt_value_sf);
            in_flight_bf_debt_sf = in_flight_bf_debt_sf.saturating_sub(observed_growth);
            allowed_borrow_value_sf = d.allowed_borrow_value_sf;
            bf_debt_value_sf = new_bf_debt;
            if let Some(sol_borrow) = d
                .borrows
                .iter()
                .find(|b| b.reserve == KAMINO_MAIN_SOL_RESERVE && b.borrowed_amount_sf > 0)
            {
                sol_value_per_lamport_sf = Some(
                    sol_borrow
                        .market_value_sf
                        .saturating_mul(1u128 << 60)
                        .saturating_div(sol_borrow.borrowed_amount_sf.max(1)),
                );
            }
        }

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

/// Pure builder for one lever-up round's instruction list. Order:
///
///   0. create-ATA-idempotent (user's wSOL ATA)
///   1..K. RefreshReserve(each obligation reserve + SOL borrow reserve)
///   K+1. RefreshObligation(multiply_obligation, obligation_reserves)
///   K+2. BorrowObligationLiquidityV2(SOL)   ← v0.1.17: farm refresh via CPI
///   K+3. spl-token CloseAccount(wSOL ATA → user wallet)
///   K+4..N. jito DepositSol ixns (typically 2: create-jitoSOL-ATA + DepositSol)
///   N+1..M. RefreshReserve(each obligation reserve + jitoSOL deposit reserve)
///   M+1. RefreshObligation(multiply_obligation, obligation_reserves)
///   M+2. DepositReserveLiquidityAndObligationCollateralV2(jitoSOL)  ← v0.1.17
///
/// klend's `check_refresh` validates that `RefreshObligation` precedes each
/// gated ixn (Borrow / Deposit). RefreshObligation itself further requires
/// EVERY reserve registered on the obligation (deposits + borrows) to have
/// been refreshed via `RefreshReserve` earlier in the same transaction,
/// otherwise it errors with Custom(0x1779) = ReserveStale (6009).
/// v0.1.14/15 only refreshed the single reserve being acted on, which broke
/// once the obligation held both a jitoSOL deposit and (post round 1) a
/// SOL borrow.
///
/// v0.1.17: switched to klend v2 handlers. The v2 borrow / deposit handlers
/// CPI into Kamino Farms internally, eliminating the manual
/// `RefreshObligationFarmsForReserve` pre/post-ix pair around each action
/// (which would otherwise be required on Kamino main market: jitoSOL has a
/// Collateral farm, SOL has a Debt farm). Reserve + obligation freshness
/// requirements are unchanged. CU limit bumped to 1_000_000 to cover the
/// added farm CPI cost.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_lever_up_ixns(
    user: Pubkey,
    lending_market: Pubkey,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
    jito_pool: &zerox1_defi_protocols::protocols::jito::StakePoolMeta,
    borrow_sol_amount: u64,
    expected_jitosol_received: u64,
    obligation_reserve_accounts: &[&ReserveAccounts],
) -> Result<Vec<Instruction>> {
    if borrow_sol_amount == 0 {
        return Err(anyhow!("borrow_sol_amount must be > 0"));
    }
    if expected_jitosol_received == 0 {
        return Err(anyhow!("expected_jitosol_received must be > 0"));
    }

    let user_wsol_ata = ata(&user, &WSOL_MINT);

    // Pubkey list passed to RefreshObligation: preserves obligation order
    // (deposits first, borrows next). Must match what's actually registered
    // on the obligation account, NOT include the action reserve unless it's
    // already a slot.
    let obligation_reserves: Vec<Pubkey> = obligation_reserve_accounts
        .iter()
        .map(|r| r.reserve)
        .collect();

    // Compute the set of reserves that must be refreshed before each
    // RefreshObligation: every reserve referenced by the obligation, plus
    // the reserve being acted on (if not already present). Preserves
    // obligation order; action reserve appended last when novel.
    fn reserves_for_refresh<'a>(
        obligation: &[&'a ReserveAccounts],
        action: &'a ReserveAccounts,
    ) -> Vec<&'a ReserveAccounts> {
        let mut out: Vec<&'a ReserveAccounts> = Vec::with_capacity(obligation.len() + 1);
        let mut seen: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
        for r in obligation {
            if seen.insert(r.reserve) {
                out.push(*r);
            }
        }
        if seen.insert(action.reserve) {
            out.push(action);
        }
        out
    }

    // Step 1: borrow SOL → user wSOL ATA via the v2 handler.
    //
    // We reuse the v1 helper's bundle to harvest its `create-ATA-idempotent`
    // first ixn (the v2 builder is bare — no ATA-create / refresh wrapping),
    // then discard the v1 RefreshReserve + v1 Borrow tail and replace them
    // with the obligation-wide RefreshReserve set + RefreshObligation + v2
    // BorrowObligationLiquidityV2. The v2 handler does the SOL-reserve
    // DebtFarm refresh via CPI, so no manual RefreshObligationFarmsForReserve
    // is needed before/after.
    let mut borrow_ixs: Vec<Instruction> = borrow_obligation_liquidity_ix(
        &user,
        sol_reserve,
        borrow_sol_amount,
        caps::MULTIPLY_OBLIGATION_SEED,
    )
    .context("build borrow_obligation_liquidity_ix (for ATA-create harvesting)")?;
    let _v1_borrow_tail = borrow_ixs.pop().expect("v1 borrow ix present");
    let _v1_refresh = borrow_ixs.pop().expect("v1 refresh-reserve ix present");
    // borrow_ixs now contains just the create-ATA-idempotent ixn.
    for r in reserves_for_refresh(obligation_reserve_accounts, sol_reserve) {
        borrow_ixs.push(refresh_reserve_ix(r));
    }
    borrow_ixs.push(refresh_obligation_ix(
        &user,
        &lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        &obligation_reserves,
    ));
    let borrow_v2 = borrow_obligation_liquidity_v2_ix(
        &user,
        sol_reserve,
        borrow_sol_amount,
        caps::MULTIPLY_OBLIGATION_SEED,
    )
    .context("build borrow_obligation_liquidity_v2_ix")?;
    borrow_ixs.push(borrow_v2);
    let mut ixs: Vec<Instruction> = borrow_ixs;

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
    let jito_ixs =
        deposit_sol_ix(&user, jito_pool, borrow_sol_amount).context("build deposit_sol_ix")?;
    ixs.extend(jito_ixs);

    // Step 4: refresh every obligation reserve (incl. jitoSOL deposit
    // reserve) + RefreshObligation + deposit collateral.
    //
    // The post-borrow RefreshObligation must include the SOL borrow reserve
    // in its remaining_accounts: the preceding BorrowObligationLiquidityV2
    // adds SOL to obligation.borrows[], so the obligation's reserve set is
    // now `deposits ++ [sol_reserve]`. On round 2+, sol_reserve is already
    // in obligation_reserves (from a previous round's borrow), so dedupe.
    let mut post_borrow_reserves: Vec<Pubkey> = obligation_reserves.clone();
    if !post_borrow_reserves.contains(&sol_reserve.reserve) {
        post_borrow_reserves.push(sol_reserve.reserve);
    }
    // v0.1.19 fix: BorrowObligationLiquidityV2 marks the SOL borrow reserve
    // stale; we MUST re-refresh it (alongside any pre-existing obligation
    // reserves and the jitoSOL deposit reserve we're about to act on) before
    // the second RefreshObligation. Build the post-borrow refresh set:
    // obligation reserves ++ [sol_reserve if novel] ++ [jitosol_reserve if novel].
    let mut post_borrow_refresh: Vec<&ReserveAccounts> =
        reserves_for_refresh(obligation_reserve_accounts, sol_reserve);
    if !post_borrow_refresh
        .iter()
        .any(|r| r.reserve == jitosol_reserve.reserve)
    {
        post_borrow_refresh.push(jitosol_reserve);
    }
    for r in post_borrow_refresh {
        ixs.push(refresh_reserve_ix(r));
    }
    ixs.push(refresh_obligation_ix(
        &user,
        &lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        &post_borrow_reserves,
    ));
    let deposit_collateral = deposit_reserve_liquidity_and_obligation_collateral_v2_ix(
        &user,
        jitosol_reserve,
        expected_jitosol_received,
        caps::MULTIPLY_OBLIGATION_SEED,
    )
    .context("build deposit_reserve_liquidity_and_obligation_collateral_v2_ix")?;
    ixs.push(deposit_collateral);

    Ok(ixs)
}

/// Build + sim/submit one leverage iteration. Lifted from the monolith's
/// `lever_up` handler body (defi-daemon/src/handlers/multiply.rs) — minus
/// the axum extractors / JSON shape / error helpers.
async fn run_one_lever_up_iteration(
    ctx: &DispatchCtx,
    user: Pubkey,
    lending_market: Pubkey,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
    jito_pool: &zerox1_defi_protocols::protocols::jito::StakePoolMeta,
    borrow_sol_amount: u64,
    expected_jitosol_received: u64,
    obligation_reserve_accounts: &[&ReserveAccounts],
) -> Result<IterationOutcome> {
    if borrow_sol_amount == 0 {
        return Err(anyhow!("borrow_sol_amount must be > 0"));
    }
    if expected_jitosol_received == 0 {
        return Err(anyhow!("expected_jitosol_received must be > 0"));
    }

    let ixs = build_lever_up_ixns(
        user,
        lending_market,
        sol_reserve,
        jitosol_reserve,
        jito_pool,
        borrow_sol_amount,
        expected_jitosol_received,
        obligation_reserve_accounts,
    )?;

    let ix_count = ixs.len();

    // Audit-fix I1: structural authority boundary. Every ixn in the
    // lever-up bundle must target a program in the daemon's signing
    // whitelist. RpcContext::build_signed will additionally prepend two
    // compute-budget ixns, which is also covered by the whitelist.
    ctx.whitelist
        .verify_ixns(&ixs)
        .context("whitelist check on lever-up ixns")?;

    // v0.1.20: feed Kamino's main-market ALT to the v0 message compiler.
    // The lever-up bundle references ~50 distinct accounts and overflows
    // the 1232-byte raw / 1644-byte base64 tx limit without lookup tables
    // (live: 1648 bytes — 4 bytes over). Kamino's ALT covers the program
    // IDs, market PDAs, and reserve auxiliary accounts, collapsing each
    // covered account from 32 inline bytes to 1 indexed byte.
    let alts = [KAMINO_MAIN_MARKET_LOOKUP_TABLE];

    if ctx.simulate_only {
        let sim = ctx
            .rpc
            .build_sign_simulate_with_alts(
                ixs,
                ctx.wallet.keypair(),
                MULTIPLY_CU_LIMIT,
                MULTIPLY_PRIORITY_FEE,
                &alts,
            )
            .await
            .context("simulate leverage tx")?;
        let (layout_valid, summary) = zerox1_defi_runtime::rpc::classify_simulation(&sim);
        info!(
            ix_count,
            layout_valid,
            summary = %summary,
            "round sim ok"
        );
        Ok(IterationOutcome { tx_signature: None })
    } else {
        let sig = ctx
            .rpc
            .build_sign_send_with_alts(
                ixs,
                ctx.wallet.keypair(),
                MULTIPLY_CU_LIMIT,
                MULTIPLY_PRIORITY_FEE,
                &alts,
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
    use zerox1_defi_protocols::constants::{JITO_STAKE_POOL, KAMINO_LEND_PROGRAM_ID};
    use zerox1_defi_protocols::protocols::jito::{derive_withdraw_authority, StakePoolMeta};
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

    fn dummy_pool() -> StakePoolMeta {
        StakePoolMeta::jito(
            derive_withdraw_authority(&JITO_STAKE_POOL),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        )
    }

    #[test]
    fn target_proximity_bps_sane() {
        assert!(TARGET_PROXIMITY_BPS > 0);
        assert!(TARGET_PROXIMITY_BPS < caps::MAX_LTV_BPS);
    }

    // ------------------------------------------------------------------
    // rc36 + rc38: clamp_borrow_to_headroom math
    // ------------------------------------------------------------------

    /// Convenience: build sf-scaled USD values from a plain float dollar
    /// amount. SF scale is 2^60.
    fn usd_sf(dollars: f64) -> u128 {
        ((dollars * (1u128 << 60) as f64) as u128).max(1)
    }

    /// SOL price (mainnet at test time ≈ $103) expressed as sf-scaled
    /// USD per lamport. 1 SOL = 1e9 lamports.
    fn sol_price_per_lamport_sf(sol_price_usd: f64) -> u128 {
        usd_sf(sol_price_usd) / 1_000_000_000
    }

    /// SOL on Kamino main — used by all production clamp callers.
    const BF_125: u32 = caps::SOL_BORROW_FACTOR_BPS;
    /// Hypothetical no-BF reserve — useful for isolating the in-flight
    /// invariant from the rc39 BF math.
    const BF_NONE: u32 = 10_000;

    #[test]
    fn clamp_falls_back_to_naive_when_no_price_ratio() {
        // First round on a fresh wallet with no prior SOL borrow has
        // `sol_value_per_lamport_sf == 0`; the clamp must fall through
        // to the naive amount so the loop can bootstrap.
        let naive = 500_000_000u64;
        let clamped = clamp_borrow_to_headroom(naive, usd_sf(10.0), usd_sf(0.0), 0, 0, BF_125);
        assert_eq!(clamped, naive);
    }

    #[test]
    fn clamp_respects_chain_headroom_no_bf_no_in_flight() {
        // BF=10000 (no adjustment) isolates the headroom math. Allowed
        // $10, bf-debt $5 → raw headroom $5, safe headroom (×0.9) =
        // $4.50. At $100/SOL that's 0.045 SOL = 45_000_000 lamports.
        let naive = 1_000_000_000u64;
        let price = sol_price_per_lamport_sf(100.0);
        let clamped = clamp_borrow_to_headroom(naive, usd_sf(10.0), usd_sf(5.0), 0, price, BF_NONE);
        let expected: u64 = 45_000_000;
        let delta = (clamped as i128 - expected as i128).unsigned_abs() as u64;
        assert!(
            delta < expected / 50,
            "clamped={clamped}, expected≈{expected}, delta={delta}"
        );
    }

    /// rc39 core invariant: a 1.25× borrow factor must shrink the
    /// per-round lamport budget by 1/1.25 = 80% versus the same scenario
    /// with no BF. Pre-rc39 the clamp ignored BF and asked the
    /// no-adjustment amount, causing live BorrowTooLarge on 2026-05-28.
    #[test]
    fn clamp_with_borrow_factor_shrinks_budget_by_bf_inverse() {
        let naive = 1_000_000_000u64;
        let price = sol_price_per_lamport_sf(100.0);
        let allowed = usd_sf(10.0);
        let bf_debt = usd_sf(5.0);

        let no_bf = clamp_borrow_to_headroom(naive, allowed, bf_debt, 0, price, BF_NONE);
        let with_bf = clamp_borrow_to_headroom(naive, allowed, bf_debt, 0, price, BF_125);

        // with_bf should be no_bf × (10000/12500) = no_bf × 0.8.
        let ratio_bps = (with_bf as u128 * 10_000) / no_bf as u128;
        assert!(
            (7_900..=8_100).contains(&(ratio_bps as u64)),
            "expected ~80% of no_bf, got {ratio_bps}bps"
        );
    }

    /// rc38 invariant: a non-zero `in_flight_bf_debt_sf` must shrink
    /// the available headroom for the next round even before the chain
    /// reflects the prior round's borrow. Verified independent of BF
    /// by using BF=10_000 here.
    #[test]
    fn clamp_pessimistically_includes_in_flight_bf_debt() {
        let naive = 1_000_000_000u64;
        let price = sol_price_per_lamport_sf(100.0);
        let allowed = usd_sf(10.0);
        let bf_debt = usd_sf(5.0);

        let no_in_flight = clamp_borrow_to_headroom(naive, allowed, bf_debt, 0, price, BF_NONE);
        let with_in_flight =
            clamp_borrow_to_headroom(naive, allowed, bf_debt, usd_sf(2.0), price, BF_NONE);

        assert!(
            with_in_flight < no_in_flight,
            "in-flight tracking must shrink headroom: no_in_flight={no_in_flight}, with_in_flight={with_in_flight}"
        );
        // Roughly 60% (2.70/4.50).
        let ratio_bps = (with_in_flight as u128 * 10_000) / no_in_flight as u128;
        assert!(
            (5_500..=6_500).contains(&(ratio_bps as u64)),
            "expected ~60% of no_in_flight, got {ratio_bps}bps"
        );
    }

    #[test]
    fn clamp_zero_headroom_returns_zero() {
        let naive = 1_000_000_000u64;
        let price = sol_price_per_lamport_sf(100.0);
        let clamped = clamp_borrow_to_headroom(naive, usd_sf(5.0), usd_sf(5.0), 0, price, BF_125);
        assert_eq!(clamped, 0);
    }

    #[test]
    fn clamp_in_flight_exceeding_headroom_returns_zero() {
        let naive = 1_000_000_000u64;
        let price = sol_price_per_lamport_sf(100.0);
        let clamped =
            clamp_borrow_to_headroom(naive, usd_sf(5.0), usd_sf(3.0), usd_sf(5.0), price, BF_125);
        assert_eq!(clamped, 0);
    }

    #[test]
    fn clamp_naive_smaller_than_headroom_returns_naive() {
        let naive = 1_000_000u64; // 0.001 SOL
        let price = sol_price_per_lamport_sf(100.0);
        let clamped = clamp_borrow_to_headroom(naive, usd_sf(10.0), usd_sf(5.0), 0, price, BF_125);
        assert_eq!(clamped, naive);
    }

    /// rc39 regression: replay the live 2026-05-28T22:19 scenario.
    /// Allowed_borrow = 7.9721 USD, bf_debt = 6.7235 USD → raw headroom
    /// 1.2486. Pre-rc39 clamp returned 13_044_077 lamports which the
    /// chain rejected. Post-rc39 clamp must return ≤ 10_435_261 lamports
    /// (the BF-adjusted equivalent at the same prices).
    #[test]
    fn clamp_matches_live_kamino_check_at_bf_125() {
        let naive = 833_333_333u64;
        let allowed = usd_sf(7.9721);
        let bf_debt = usd_sf(6.7235);
        // Chain: 5.3788 USD / 65473688 lamports = 8.215e-8 USD/lamport
        let price = ((5.3788_f64 * (1u128 << 60) as f64) / 65_473_688_f64) as u128;

        let clamped = clamp_borrow_to_headroom(naive, allowed, bf_debt, 0, price, BF_125);

        // The chain rejects when bf_value > allowed_borrow - existing_bf_debt
        // i.e. > $1.2486. Assert the clamp keeps us comfortably under that
        // ceiling. The 0.9 safety factor SHOULD land us near $1.1237 but
        // FP ε at the boundary makes equality flaky — just check the
        // hard chain limit.
        let bf_value_usd = (clamped as f64) * 5.3788 / 65_473_688.0 * 1.25;
        assert!(
            bf_value_usd < 1.2486,
            "clamped {clamped} lamports → bf_value ${bf_value_usd:.4} exceeds chain max $1.2486"
        );
        // Should be ~$1.12 (=0.9 × 1.2486), confirming the 10% safety
        // factor is in the right ballpark.
        assert!(
            (1.10..=1.13).contains(&bf_value_usd),
            "expected bf_value ≈ $1.12 (0.9 × $1.2486), got ${bf_value_usd:.4}"
        );
        assert!(clamped > 0, "expected non-zero clamp with real headroom");
    }

    #[test]
    fn cu_limit_sane() {
        // klend deposit + borrow + jito DepositSol + refresh + v2 farm CPIs
        // fit well under 1.4M. v0.1.17 bumped from 800k to 1M to cover the
        // added farm CPI cost (CollateralFarm on jitoSOL + DebtFarm on SOL).
        assert!(MULTIPLY_CU_LIMIT >= 1_000_000);
        assert!(MULTIPLY_CU_LIMIT < 1_400_000);
    }

    /// v0.1.14 regression: klend rejects BorrowObligationLiquidity (and
    /// DepositObligationCollateral) unless the immediately preceding ixn is
    /// `RefreshObligation` for the same obligation. Assert the bundle's
    /// program-IDs + discriminators are positioned correctly.
    #[test]
    fn lever_up_bundle_has_refresh_obligation_before_borrow_and_deposit() {
        let user = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let sol_reserve = dummy_reserve(lending_market, WSOL_MINT);
        let jitosol_reserve = dummy_reserve(lending_market, JITOSOL_MINT);
        let jito_pool = dummy_pool();
        let obligation_reserve_accounts: Vec<&ReserveAccounts> = vec![&jitosol_reserve];

        let ixs = build_lever_up_ixns(
            user,
            lending_market,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            1_000_000_000,
            900_000_000,
            &obligation_reserve_accounts,
        )
        .expect("build lever-up bundle");

        let refresh_obligation_disc = anchor_discriminator("global", "refresh_obligation");
        // v0.1.17: bundle uses v2 handlers (CPI-internal farm refresh).
        let borrow_disc = anchor_discriminator("global", "borrow_obligation_liquidity_v2");
        let deposit_disc = anchor_discriminator(
            "global",
            "deposit_reserve_liquidity_and_obligation_collateral_v2",
        );

        let is_refresh_obligation = |ix: &Instruction| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == refresh_obligation_disc
        };
        let is_borrow = |ix: &Instruction| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == borrow_disc
        };
        let is_deposit_collateral = |ix: &Instruction| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == deposit_disc
        };

        let borrow_idx = ixs
            .iter()
            .position(is_borrow)
            .expect("borrow ixn present in bundle");
        assert!(borrow_idx > 0, "borrow at index 0 — no room for refresh");
        assert!(
            is_refresh_obligation(&ixs[borrow_idx - 1]),
            "ixn directly before BorrowObligationLiquidity (idx {borrow_idx}) must be \
             RefreshObligation; got program {} disc {:?}",
            ixs[borrow_idx - 1].program_id,
            &ixs[borrow_idx - 1].data.get(..8)
        );

        let deposit_idx = ixs
            .iter()
            .position(is_deposit_collateral)
            .expect("deposit_obligation_collateral ixn present in bundle");
        assert!(deposit_idx > 0, "deposit at index 0 — no room for refresh");
        assert!(
            is_refresh_obligation(&ixs[deposit_idx - 1]),
            "ixn directly before DepositObligationCollateral (idx {deposit_idx}) must be \
             RefreshObligation; got program {} disc {:?}",
            ixs[deposit_idx - 1].program_id,
            &ixs[deposit_idx - 1].data.get(..8)
        );

        // Borrow comes before deposit (the round walks borrow → swap → deposit).
        assert!(
            borrow_idx < deposit_idx,
            "borrow ({borrow_idx}) must precede deposit ({deposit_idx})"
        );
    }

    /// v0.1.16 regression: klend's RefreshObligation requires every reserve
    /// in the obligation (deposits + borrows) to be refreshed via
    /// RefreshReserve in the same transaction (Custom(0x1779) = ReserveStale).
    /// Given an obligation with a single jitoSOL deposit, lever-up bundle
    /// must contain BOTH `RefreshReserve(jitoSOL)` AND `RefreshReserve(SOL)`
    /// before the first `RefreshObligation`.
    #[test]
    fn lever_up_bundle_refreshes_all_obligation_reserves_before_refresh_obligation() {
        let user = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let sol_reserve = dummy_reserve(lending_market, WSOL_MINT);
        let jitosol_reserve = dummy_reserve(lending_market, JITOSOL_MINT);
        let jito_pool = dummy_pool();
        // Obligation already holds a jitoSOL deposit (post-seed); the
        // action is to borrow from a different reserve (SOL).
        let obligation_reserve_accounts: Vec<&ReserveAccounts> = vec![&jitosol_reserve];

        let ixs = build_lever_up_ixns(
            user,
            lending_market,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            1_000_000_000,
            900_000_000,
            &obligation_reserve_accounts,
        )
        .expect("build lever-up bundle");

        let refresh_reserve_disc = anchor_discriminator("global", "refresh_reserve");
        let refresh_obligation_disc = anchor_discriminator("global", "refresh_obligation");

        let is_refresh_reserve_of = |ix: &Instruction, reserve: &Pubkey| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == refresh_reserve_disc
                && !ix.accounts.is_empty()
                && ix.accounts[0].pubkey == *reserve
        };
        let is_refresh_obligation = |ix: &Instruction| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == refresh_obligation_disc
        };

        let first_refresh_obligation_idx = ixs
            .iter()
            .position(is_refresh_obligation)
            .expect("RefreshObligation present in bundle");

        // BOTH the jitoSOL collateral reserve AND the SOL borrow reserve
        // must be refreshed before the first RefreshObligation.
        let jitosol_refreshed = ixs[..first_refresh_obligation_idx]
            .iter()
            .any(|ix| is_refresh_reserve_of(ix, &jitosol_reserve.reserve));
        let sol_refreshed = ixs[..first_refresh_obligation_idx]
            .iter()
            .any(|ix| is_refresh_reserve_of(ix, &sol_reserve.reserve));

        assert!(
            jitosol_refreshed,
            "RefreshReserve(jitoSOL) must appear before first RefreshObligation (idx {first_refresh_obligation_idx})"
        );
        assert!(
            sol_refreshed,
            "RefreshReserve(SOL) must appear before first RefreshObligation (idx {first_refresh_obligation_idx})"
        );
    }

    /// v0.1.18 regression: after BorrowObligationLiquidityV2 lands, the
    /// obligation has deposits=[jitoSOL] + borrows=[SOL]. The post-borrow
    /// RefreshObligation (the one in front of DepositObligationCollateral)
    /// must therefore include the SOL reserve in its remaining_accounts —
    /// otherwise klend rejects with InvalidAccountInput (6006):
    /// `expected_remaining_accounts=2, actual_remaining_accounts=1`.
    ///
    /// `refresh_obligation_ix` builds accounts = [lending_market, obligation,
    /// ...obligation_reserves]; remaining_accounts therefore start at idx 2.
    #[test]
    fn second_refresh_obligation_includes_sol_borrow_reserve() {
        let user = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let sol_reserve = dummy_reserve(lending_market, WSOL_MINT);
        let jitosol_reserve = dummy_reserve(lending_market, JITOSOL_MINT);
        let jito_pool = dummy_pool();

        let refresh_obligation_disc = anchor_discriminator("global", "refresh_obligation");
        let is_refresh_obligation = |ix: &Instruction| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == refresh_obligation_disc
        };

        // Round 1: obligation has only jitoSOL deposit. The post-borrow
        // RefreshObligation must include [jitoSOL, SOL] (deposits, borrows).
        let round1_reserves: Vec<&ReserveAccounts> = vec![&jitosol_reserve];
        let ixs_round1 = build_lever_up_ixns(
            user,
            lending_market,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            1_000_000_000,
            900_000_000,
            &round1_reserves,
        )
        .expect("build lever-up bundle (round 1)");
        let refresh_obligation_idxs: Vec<usize> = ixs_round1
            .iter()
            .enumerate()
            .filter_map(|(i, ix)| {
                if is_refresh_obligation(ix) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            refresh_obligation_idxs.len(),
            2,
            "expected exactly 2 RefreshObligation ixs in lever-up bundle"
        );
        let second = &ixs_round1[refresh_obligation_idxs[1]];
        let remaining: Vec<Pubkey> = second.accounts[2..].iter().map(|m| m.pubkey).collect();
        assert_eq!(
            remaining,
            vec![jitosol_reserve.reserve, sol_reserve.reserve],
            "round 1 post-borrow RefreshObligation remaining_accounts must be \
             [jitoSOL_reserve, sol_reserve] (deposits first, borrows second)"
        );

        // Round 2+: obligation already has jitoSOL deposit + SOL borrow. The
        // post-borrow RefreshObligation must NOT duplicate sol_reserve.
        // We simulate the on-chain reserve list as [jitoSOL, SOL] — what
        // multiply.rs would feed in after decoding the obligation account.
        let round2_reserves: Vec<&ReserveAccounts> = vec![&jitosol_reserve, &sol_reserve];
        let ixs_round2 = build_lever_up_ixns(
            user,
            lending_market,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            1_000_000_000,
            900_000_000,
            &round2_reserves,
        )
        .expect("build lever-up bundle (round 2+)");
        let refresh_obligation_idxs_r2: Vec<usize> = ixs_round2
            .iter()
            .enumerate()
            .filter_map(|(i, ix)| {
                if is_refresh_obligation(ix) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        let second_r2 = &ixs_round2[refresh_obligation_idxs_r2[1]];
        let remaining_r2: Vec<Pubkey> = second_r2.accounts[2..].iter().map(|m| m.pubkey).collect();
        assert_eq!(
            remaining_r2,
            vec![jitosol_reserve.reserve, sol_reserve.reserve],
            "round 2+ post-borrow RefreshObligation must be [jitoSOL, SOL] with \
             no duplication (sol_reserve already present pre-borrow)"
        );
    }

    /// v0.1.21 regression: between rounds, the leverage loop tracks the
    /// obligation's reserve set locally instead of re-fetching from RPC
    /// (which races read-replica lag and was observed returning the
    /// pre-borrow obligation state on round 2). Simulate the local-bump
    /// state transition `[jitoSOL] -> [jitoSOL, SOL]` and assert that:
    ///   - the bumped set contains both reserves with no duplicates;
    ///   - the round-2 bundle built from the bumped set's second
    ///     RefreshObligation has `[jitoSOL, SOL]` in remaining_accounts
    ///     (i.e. doesn't drop SOL on the way to the round-2 borrow).
    #[test]
    fn local_obligation_reserve_bump_persists_sol_borrow_across_rounds() {
        let user = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let sol_reserve = dummy_reserve(lending_market, WSOL_MINT);
        let jitosol_reserve = dummy_reserve(lending_market, JITOSOL_MINT);
        let jito_pool = dummy_pool();

        // Round 1 starting state: only the seeded jitoSOL deposit.
        let mut obligation_reserves: Vec<Pubkey> = vec![jitosol_reserve.reserve];

        // Apply the same local bump the loop performs after a successful
        // broadcast.
        if !obligation_reserves.contains(&jitosol_reserve.reserve) {
            obligation_reserves.push(jitosol_reserve.reserve);
        }
        if !obligation_reserves.contains(&sol_reserve.reserve) {
            obligation_reserves.push(sol_reserve.reserve);
        }

        // The bumped set must contain BOTH reserves (jitoSOL deposit +
        // SOL borrow) with no duplication.
        assert_eq!(
            obligation_reserves,
            vec![jitosol_reserve.reserve, sol_reserve.reserve],
            "after one round, locally-bumped obligation_reserves must be \
             [jitoSOL, SOL] (deposit first, borrow second, no dupes)"
        );

        // Round 2: build a bundle with the bumped reserve set. The second
        // RefreshObligation's remaining_accounts must reflect both reserves;
        // if local tracking dropped SOL (the v0.1.20 RPC-lag bug), this
        // would assert-fail with a single-element vector.
        let round2_accounts: Vec<&ReserveAccounts> = vec![&jitosol_reserve, &sol_reserve];
        let ixs = build_lever_up_ixns(
            user,
            lending_market,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            1_000_000_000,
            900_000_000,
            &round2_accounts,
        )
        .expect("build lever-up bundle (round 2 with bumped state)");

        let refresh_obligation_disc = anchor_discriminator("global", "refresh_obligation");
        let is_refresh_obligation = |ix: &Instruction| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == refresh_obligation_disc
        };
        let refresh_obligation_idxs: Vec<usize> = ixs
            .iter()
            .enumerate()
            .filter_map(|(i, ix)| {
                if is_refresh_obligation(ix) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(refresh_obligation_idxs.len(), 2);
        // Both pre-borrow and post-borrow RefreshObligation must carry
        // [jitoSOL, SOL] — pre-borrow because the obligation already holds
        // the SOL borrow from round 1, post-borrow because the borrow ixn
        // doesn't add a NEW reserve (SOL is already present, so dedupe).
        for idx in &refresh_obligation_idxs {
            let ix = &ixs[*idx];
            let remaining: Vec<Pubkey> = ix.accounts[2..].iter().map(|m| m.pubkey).collect();
            assert_eq!(
                remaining,
                vec![jitosol_reserve.reserve, sol_reserve.reserve],
                "round-2 RefreshObligation at idx {idx} must carry \
                 [jitoSOL, SOL] from the locally-bumped reserve set"
            );
        }
    }

    /// v0.1.19 regression: after BorrowObligationLiquidityV2 lands, klend
    /// marks the SOL borrow reserve stale. The next RefreshObligation
    /// (pre-deposit) requires every listed reserve — including SOL — to have
    /// been refreshed via RefreshReserve in the same transaction, AFTER the
    /// borrow. Otherwise klend errors with Custom(6009) = ReserveStale.
    ///
    /// Assert: between the borrow ixn and the second RefreshObligation, both
    /// RefreshReserve(jitoSOL) AND RefreshReserve(SOL) appear.
    #[test]
    fn post_borrow_refresh_loop_refreshes_jitosol_and_sol() {
        let user = Pubkey::new_unique();
        let lending_market = Pubkey::new_unique();
        let sol_reserve = dummy_reserve(lending_market, WSOL_MINT);
        let jitosol_reserve = dummy_reserve(lending_market, JITOSOL_MINT);
        let jito_pool = dummy_pool();

        let borrow_disc = anchor_discriminator("global", "borrow_obligation_liquidity_v2");
        let refresh_reserve_disc = anchor_discriminator("global", "refresh_reserve");
        let refresh_obligation_disc = anchor_discriminator("global", "refresh_obligation");
        let is_borrow = |ix: &Instruction| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == borrow_disc
        };
        let is_refresh_reserve_of = |ix: &Instruction, reserve: &Pubkey| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == refresh_reserve_disc
                && !ix.accounts.is_empty()
                && ix.accounts[0].pubkey == *reserve
        };
        let is_refresh_obligation = |ix: &Instruction| {
            ix.program_id == KAMINO_LEND_PROGRAM_ID
                && ix.data.len() >= 8
                && ix.data[..8] == refresh_obligation_disc
        };

        // Round 1: obligation has only jitoSOL deposit pre-borrow.
        let obligation_reserve_accounts: Vec<&ReserveAccounts> = vec![&jitosol_reserve];
        let ixs = build_lever_up_ixns(
            user,
            lending_market,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            1_000_000_000,
            900_000_000,
            &obligation_reserve_accounts,
        )
        .expect("build lever-up bundle");

        let borrow_idx = ixs.iter().position(is_borrow).expect("borrow ixn present");
        let refresh_obligation_idxs: Vec<usize> = ixs
            .iter()
            .enumerate()
            .filter_map(|(i, ix)| {
                if is_refresh_obligation(ix) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(refresh_obligation_idxs.len(), 2);
        let second_refresh_obligation_idx = refresh_obligation_idxs[1];
        assert!(
            borrow_idx < second_refresh_obligation_idx,
            "borrow ({borrow_idx}) must precede second RefreshObligation \
             ({second_refresh_obligation_idx})"
        );

        let window = &ixs[borrow_idx + 1..second_refresh_obligation_idx];
        let jitosol_refreshed = window
            .iter()
            .any(|ix| is_refresh_reserve_of(ix, &jitosol_reserve.reserve));
        let sol_refreshed = window
            .iter()
            .any(|ix| is_refresh_reserve_of(ix, &sol_reserve.reserve));

        assert!(
            jitosol_refreshed,
            "RefreshReserve(jitoSOL) must appear between borrow (idx {borrow_idx}) and \
             second RefreshObligation (idx {second_refresh_obligation_idx})"
        );
        assert!(
            sol_refreshed,
            "RefreshReserve(SOL) must appear between borrow (idx {borrow_idx}) and \
             second RefreshObligation (idx {second_refresh_obligation_idx}) — \
             klend marks the borrow reserve stale after BorrowObligationLiquidityV2 \
             and the pre-deposit RefreshObligation will otherwise fail with \
             Custom(6009) = ReserveStale"
        );
    }
}
