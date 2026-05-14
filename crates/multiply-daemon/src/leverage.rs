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
        JITOSOL_MINT, KAMINO_MAIN_JITOSOL_RESERVE, KAMINO_MAIN_MARKET, KAMINO_MAIN_SOL_RESERVE,
        TOKEN_PROGRAM_ID, WSOL_MINT,
    },
    protocols::{
        jito::deposit_sol_ix,
        jito_loader::load_jito_pool,
        kamino::{
            borrow_obligation_liquidity_ix, deposit_collateral_only_ix,
            derive_user_obligation_with_seed, refresh_obligation_ix, refresh_reserve_ix,
            ReserveAccounts,
        },
        kamino_loader::{fetch_obligation, load_reserve, query_position_ltv_bps},
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

    // v0.1.11 Bug 1 fix: seed the obligation with an initial jitoSOL
    // deposit before the leverage loop's first borrow. On a fresh wallet,
    // round 1's borrow_obligation_liquidity_ix would fail with Custom(6051)
    // (zero collateral). The seed is a no-op when the obligation already
    // holds collateral. Sim-only mode simulates the seed bundle but does
    // not broadcast.
    let seeded = crate::seed::maybe_seed_obligation(ctx)
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

        // v0.1.14 fix: fetch the obligation's currently-registered reserves
        // and pass them to RefreshObligation. klend's check_refresh requires
        // a RefreshObligation immediately before BOTH BorrowObligationLiquidity
        // AND DepositObligationCollateral, with `remaining_accounts` matching
        // the obligation's deposit/borrow slots in slot order. After the v0.1.13
        // seed deposit, that's `[jitoSOL reserve]`; after round 1's borrow,
        // klend will also have registered the SOL reserve as a borrow.
        let obligation_addr = derive_user_obligation_with_seed(
            &user,
            &lending_market,
            caps::MULTIPLY_OBLIGATION_SEED.0,
            caps::MULTIPLY_OBLIGATION_SEED.1,
        );
        let decoded = fetch_obligation(&ctx.rpc.client, &obligation_addr)
            .await
            .context("fetch obligation for refresh remaining accounts")?;
        let obligation_reserves: Vec<Pubkey> = decoded
            .as_ref()
            .map(|d| {
                d.deposits
                    .iter()
                    .map(|x| x.reserve)
                    .chain(d.borrows.iter().map(|x| x.reserve))
                    .collect()
            })
            .unwrap_or_default();

        let outcome = run_one_lever_up_iteration(
            ctx,
            user,
            lending_market,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            per_round_borrow_lamports,
            expected_jitosol_received,
            &obligation_reserves,
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

/// Pure builder for one lever-up round's instruction list. Order:
///
///   0. create-ATA-idempotent (user's wSOL ATA)
///   1. RefreshReserve(SOL)
///   2. RefreshObligation(multiply_obligation, obligation_reserves)   ← v0.1.14
///   3. BorrowObligationLiquidity(SOL)
///   4. spl-token CloseAccount(wSOL ATA → user wallet)
///   5..N. jito DepositSol ixns (typically 2: create-jitoSOL-ATA + DepositSol)
///   N+1. RefreshReserve(jitoSOL)
///   N+2. RefreshObligation(multiply_obligation, obligation_reserves)  ← v0.1.14
///   N+3. DepositObligationCollateral(jitoSOL cTokens)
///
/// klend's `check_refresh` validates `required_pre_ixs` in *reverse* order
/// relative to the gated ixn (Borrow or Deposit). For both Borrow and Deposit,
/// that means:
///   ix-1 = RefreshObligation
///   ix-2 = RefreshReserve
/// The v0.1.13 bundle had only RefreshReserve at ix-1 before Borrow, which
/// is why klend rejected with Custom(0x17a3) = IncorrectInstructionInPosition
/// once the obligation had a deposit slot to validate (the seed succeeded
/// but registered a deposit, making RefreshObligation mandatory thereafter).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_lever_up_ixns(
    user: Pubkey,
    lending_market: Pubkey,
    sol_reserve: &ReserveAccounts,
    jitosol_reserve: &ReserveAccounts,
    jito_pool: &zerox1_defi_protocols::protocols::jito::StakePoolMeta,
    borrow_sol_amount: u64,
    expected_jitosol_received: u64,
    obligation_reserves: &[Pubkey],
) -> Result<Vec<Instruction>> {
    if borrow_sol_amount == 0 {
        return Err(anyhow!("borrow_sol_amount must be > 0"));
    }
    if expected_jitosol_received == 0 {
        return Err(anyhow!("expected_jitosol_received must be > 0"));
    }

    let user_wsol_ata = ata(&user, &WSOL_MINT);

    // Step 1: borrow SOL → user wSOL ATA. The helper returns
    // `[ATA-create-idempotent, RefreshReserve(SOL), Borrow]`. Splice a
    // RefreshObligation between the RefreshReserve and the borrow.
    let mut borrow_ixs: Vec<Instruction> = borrow_obligation_liquidity_ix(
        &user,
        sol_reserve,
        borrow_sol_amount,
        caps::MULTIPLY_OBLIGATION_SEED,
    )
    .context("build borrow_obligation_liquidity_ix")?;
    let borrow_tail = borrow_ixs.pop().expect("borrow ix present");
    borrow_ixs.push(refresh_obligation_ix(
        &user,
        &lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        obligation_reserves,
    ));
    borrow_ixs.push(borrow_tail);
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

    // Step 4: refresh jitoSOL reserve + RefreshObligation + deposit collateral.
    // Same check_refresh rule as the borrow side: RefreshObligation must be
    // the ixn immediately preceding DepositObligationCollateral.
    ixs.push(refresh_reserve_ix(jitosol_reserve));
    ixs.push(refresh_obligation_ix(
        &user,
        &lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        obligation_reserves,
    ));
    let deposit_collateral = deposit_collateral_only_ix(
        &user,
        jitosol_reserve,
        expected_jitosol_received,
        caps::MULTIPLY_OBLIGATION_SEED,
    )
    .context("build deposit_collateral_only_ix")?;
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
    obligation_reserves: &[Pubkey],
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
        obligation_reserves,
    )?;

    let ix_count = ixs.len();

    // Audit-fix I1: structural authority boundary. Every ixn in the
    // lever-up bundle must target a program in the daemon's signing
    // whitelist. RpcContext::build_signed will additionally prepend two
    // compute-budget ixns, which is also covered by the whitelist.
    ctx.whitelist
        .verify_ixns(&ixs)
        .context("whitelist check on lever-up ixns")?;

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

    #[test]
    fn cu_limit_sane() {
        // klend deposit + borrow + jito DepositSol + refresh fits well under 1.4M.
        assert!(MULTIPLY_CU_LIMIT > 400_000);
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
        let obligation_reserves = vec![jitosol_reserve.reserve];

        let ixs = build_lever_up_ixns(
            user,
            lending_market,
            &sol_reserve,
            &jitosol_reserve,
            &jito_pool,
            1_000_000_000,
            900_000_000,
            &obligation_reserves,
        )
        .expect("build lever-up bundle");

        let refresh_obligation_disc = anchor_discriminator("global", "refresh_obligation");
        let borrow_disc = anchor_discriminator("global", "borrow_obligation_liquidity");
        let deposit_disc = anchor_discriminator(
            "global",
            "deposit_reserve_liquidity_and_obligation_collateral",
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
}
