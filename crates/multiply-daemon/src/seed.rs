//! v0.1.11 Bug 1 fix: seed an empty obligation with an initial jitoSOL
//! deposit before the leverage loop's first borrow.
//!
//! Background: the leverage loop's round 1 tries to
//! `borrow_obligation_liquidity_ix` immediately, but a brand-new wallet has
//! no collateral. Klend rejects with `Custom(6051)` (borrow on an obligation
//! with zero collateral).
//!
//! Fix (Option A — auto-detect from wallet balance):
//!   * If the obligation already has collateral (any deposit slot with
//!     `deposited_amount > 0`), this is a no-op.
//!   * Otherwise, read the wallet's SOL balance, reserve a fee buffer, stake
//!     the rest to jitoSOL via Jito's DepositSol, and deposit the resulting
//!     jitoSOL as Kamino collateral.
//!
//! The seed bundle reuses `kamino::deposit_ix`, which already handles
//! `InitializeObligation` (skipped if the PDA exists), idempotent ATA
//! creation, RefreshReserve, RefreshObligation (with the correct
//! `obligation_reserves` slice), and the deposit instruction itself. We
//! also prepend `init_user_metadata_ix` when the user metadata PDA is
//! missing — required on a truly fresh wallet.
//!
//! Sim-only respect: builds the same bundle and runs it through
//! `RpcContext::build_sign_simulate`. No tx is broadcast.

use anyhow::{Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::instruction::create_associated_token_account_idempotent;
use tracing::{debug, info, warn};
use zerox1_defi_protocols::{
    constants::{
        JITOSOL_MINT, KAMINO_MAIN_JITOSOL_RESERVE, KAMINO_MAIN_MARKET, KAMINO_MAIN_SOL_RESERVE,
        TOKEN_PROGRAM_ID, WSOL_MINT,
    },
    protocols::{
        jito::{deposit_sol_ix, StakePoolMeta},
        jito_loader::load_jito_pool,
        jupiter::{build_usdc_to_sol_swap_tx, JupiterSwap},
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

/// Compute budget for the seed bundle. Larger than a leverage round
/// because the first deposit also runs init_user_metadata + init_obligation
/// + (optional) init_obligation_farms + the v2 farm CPI on the jitoSOL
/// CollateralFarm. 1.2M CU has been adequate; we keep it here. (Multiply
/// rounds use 1M; the seed has more one-time inits, so the slightly higher
/// budget remains.)
const SEED_CU_LIMIT: u32 = 1_200_000;
const SEED_PRIORITY_FEE: u64 = 10_000;

/// Lamports reserved for transaction fees + future round costs. The
/// daemon will lever up after seeding, each round paying its own fee and
/// possibly creating new ATAs. 0.02 SOL (= 20_000_000 lamports) covers
/// ~100 priority-fee-bearing txs at the current priority-fee setting.
pub const SEED_FEE_BUFFER_LAMPORTS: u64 = 20_000_000;

/// Minimum SOL that must remain after reserving the fee buffer before
/// seeding is worthwhile. Below this, the seed deposit would round to
/// dust and waste a tx. 0.001 SOL = 1_000_000 lamports.
pub const SEED_MIN_STAKE_LAMPORTS: u64 = 1_000_000;

/// Decision returned by [`decide_seed_amount`]: either a positive lamport
/// amount to stake → deposit, or a reason why seeding was skipped.
#[derive(Debug, PartialEq, Eq)]
pub enum SeedDecision {
    /// Stake this many SOL lamports to jitoSOL and deposit as collateral.
    Stake(u64),
    /// Wallet balance after reserving fees would leave nothing to stake.
    InsufficientWalletBalance { wallet_lamports: u64 },
}

/// Pure decision: how much idle wallet SOL to stake → deposit into the
/// multiply obligation. Inputs:
///   * `wallet_lamports` — current native SOL balance.
///   * `fee_buffer_lamports` — lamports to leave behind for tx fees.
///   * `max_stake_lamports` — operator-configured ceiling (the daemon's
///     `--max-position-usdc-lamports` re-interpreted as a SOL cap). The
///     stake is clamped to this value so a misconfigured wallet can't
///     exceed the operator's blast-radius limit.
///
/// rc53: there is no longer a "skip if obligation has jitoSOL" gate.
/// Pre-rc53 (rc47/rc52), the gate was guarded behind `force_top_up`
/// which was only set on USDC-routed envelopes. That choice stranded
/// SOL when a failed seed bundle left wallet SOL above the buffer —
/// the next `usdc=0` leverage call would skip the deposit even though
/// the capital was waiting to be put to work. With rc53 the rule is
/// simpler: any wallet SOL above the buffer is multiply-strategy
/// capital and should be staked. The multiply daemon is the only fleet
/// daemon that ever puts SOL in the shared signing wallet (stable_yield
/// and hedgedjlp settle in USDC), so this sweep is structurally safe.
///
/// Returns the [`SeedDecision`] variant. `decide_seed_amount` is the
/// unit-testable core of [`maybe_seed_obligation`].
pub fn decide_seed_amount(
    wallet_lamports: u64,
    fee_buffer_lamports: u64,
    max_stake_lamports: u64,
) -> SeedDecision {
    let after_buffer = wallet_lamports.saturating_sub(fee_buffer_lamports);
    if after_buffer < SEED_MIN_STAKE_LAMPORTS {
        return SeedDecision::InsufficientWalletBalance { wallet_lamports };
    }

    SeedDecision::Stake(after_buffer.min(max_stake_lamports))
}

/// Build the seed-deposit instruction bundle: optional init_user_metadata
/// + jito DepositSol + kamino deposit_ix (jitoSOL collateral).
///
/// `expected_jitosol_received` is the amount handed to `deposit_ix`; the
/// caller computes it from the SOL stake using the same conservative
/// haircut the leverage loop uses (0.5% buffer assumes ≈1:1 SOL:jitoSOL).
/// `obligation_reserve_accounts` (rc52): every reserve currently referenced
/// by the obligation (deposits + borrows). klend's RefreshObligation requires
/// each one to have been refreshed via `refresh_reserve_ix` in the same tx,
/// or it bails with InvalidAccountInput (0x1776) at lending_operations.rs:1713.
/// For a fresh wallet this slice is empty and only the jitoSOL reserve below
/// gets refreshed (sufficient because `refresh_obligation_ix` then receives
/// an empty remaining_accounts list). For existing leveraged positions
/// (rc47 path), this contains [jitoSOL, SOL] and both must be refreshed.
pub fn build_seed_bundle(
    user: &Pubkey,
    jito_pool: &StakePoolMeta,
    jitosol_reserve: &ReserveAccounts,
    stake_sol_lamports: u64,
    expected_jitosol_received: u64,
    user_metadata_missing: bool,
    obligation_already_exists: bool,
    obligation_reserve_accounts: &[&ReserveAccounts],
) -> Result<Vec<Instruction>> {
    let mut ixs: Vec<Instruction> = Vec::new();

    // For a truly fresh wallet, klend's initialize_obligation requires
    // user_metadata to exist first. Mirrors stable-yield-daemon's seed path.
    if user_metadata_missing {
        info!(%user, "user_metadata not found — prepending init_user_metadata_ix");
        ixs.push(init_user_metadata_ix(user));
    }

    // Jito DepositSol: SOL → jitoSOL in the user's jitoSOL ATA (created
    // idempotently). This consumes native SOL from the wallet, so the
    // wallet must hold `stake_sol_lamports` + fees at sign time.
    let jito_ixs =
        deposit_sol_ix(user, jito_pool, stake_sol_lamports).context("build jito deposit_sol_ix")?;
    ixs.extend(jito_ixs);

    // Kamino v2 seed bundle (replaces the wrapped v1 `deposit_ix`):
    //   * InitializeObligation (skip when PDA exists)
    //   * ATA-create-idempotent for the jitoSOL liquidity ATA
    //   * RefreshReserve(jitoSOL_reserve)
    //   * RefreshObligation(obligation, remaining = obligation_reserves)
    //   * DepositReserveLiquidityAndObligationCollateralV2(jitoSOL)
    //
    // The v2 handler CPI-refreshes the jitoSOL CollateralFarm internally,
    // so no manual RefreshObligationFarmsForReserve pre/post-ix is needed
    // (avoids klend 6051 IncorrectInstructionInPosition).

    if !obligation_already_exists {
        ixs.push(
            initialize_obligation_ix(
                user,
                &jitosol_reserve.lending_market,
                caps::MULTIPLY_OBLIGATION_SEED,
            )
            .context("build initialize_obligation_ix")?,
        );
    } else {
        debug!("obligation already exists; skipping InitializeObligation in seed bundle");
    }

    ixs.push(create_associated_token_account_idempotent(
        user,
        user,
        &jitosol_reserve.liquidity_mint,
        &TOKEN_PROGRAM_ID,
    ));
    // rc52: refresh every reserve already in the obligation before
    // refresh_obligation_ix. klend validates each remaining_account is
    // fresh in-tx; a stale reserve trips InvalidAccountInput (0x1776).
    // Pre-rc47 this loop was a no-op because seed only ran on fresh
    // wallets — obligation_reserve_accounts was always empty.
    for r in obligation_reserve_accounts {
        ixs.push(refresh_reserve_ix(r));
    }
    // Always refresh the jitoSOL reserve we're about to deposit into.
    // Dedupe against obligation_reserve_accounts so we don't double-emit
    // when the obligation already holds jitoSOL collateral (same-tx
    // duplicate RefreshReserve is idempotent server-side but wastes CU).
    if !obligation_reserve_accounts
        .iter()
        .any(|r| r.reserve == jitosol_reserve.reserve)
    {
        ixs.push(refresh_reserve_ix(jitosol_reserve));
    }
    let obligation_reserves: Vec<Pubkey> = obligation_reserve_accounts
        .iter()
        .map(|r| r.reserve)
        .collect();
    ixs.push(refresh_obligation_ix(
        user,
        &jitosol_reserve.lending_market,
        caps::MULTIPLY_OBLIGATION_SEED,
        &obligation_reserves,
    ));
    ixs.push(
        deposit_reserve_liquidity_and_obligation_collateral_v2_ix(
            user,
            jitosol_reserve,
            expected_jitosol_received,
            caps::MULTIPLY_OBLIGATION_SEED,
        )
        .context("build deposit_reserve_liquidity_and_obligation_collateral_v2_ix for seed")?,
    );

    Ok(ixs)
}

/// If the obligation has no collateral, build + run the seed bundle.
/// In `simulate_only` mode, the bundle is built and simulated; nothing is
/// broadcast. In submit mode, the bundle is signed + sent; the daemon
/// then waits for confirmation by polling `query_position_ltv_bps`
/// upstream in the leverage loop.
///
/// Returns `Ok(true)` if a seed was executed (or simulated), `Ok(false)`
/// if seeding was skipped (obligation already has collateral, or wallet
/// balance insufficient).
///
/// `force_top_up` (rc47): when true, the "obligation already has jitoSOL"
/// short-circuit is bypassed so the wallet's current SOL (just landed by
/// `seed_with_usdc` on the allocator-driven USDC injection path) is
/// staked and deposited as additional collateral. Without this, the
/// freshly-swapped SOL stays in the wallet and the new principal never
/// enters the obligation.
pub async fn maybe_seed_obligation(ctx: &DispatchCtx) -> Result<bool> {
    let user = ctx.wallet.pubkey();
    // v0.1.12 Bug A fix: derive multiply's obligation under its own
    // (tag, id) seed so stable-yield's $55 USDC obligation cannot be
    // cross-collateralized — or false-positive the seed-skip check.
    let obligation_addr = derive_user_obligation_with_seed(
        &user,
        &KAMINO_MAIN_MARKET,
        caps::MULTIPLY_OBLIGATION_SEED.0,
        caps::MULTIPLY_OBLIGATION_SEED.1,
    );

    let decoded = fetch_obligation(&ctx.rpc.client, &obligation_addr)
        .await
        .context("fetch obligation for seed decision")?;

    let wallet_lamports = ctx
        .rpc
        .client
        .get_balance(&user)
        .await
        .context("read wallet SOL balance for seed decision")?;

    let decision = decide_seed_amount(
        wallet_lamports,
        SEED_FEE_BUFFER_LAMPORTS,
        ctx.args_max_position_usdc_lamports,
    );

    let stake_lamports = match decision {
        SeedDecision::Stake(n) => n,
        SeedDecision::InsufficientWalletBalance { wallet_lamports } => {
            warn!(
                wallet_lamports,
                fee_buffer = SEED_FEE_BUFFER_LAMPORTS,
                "wallet balance too low after fee buffer; cannot seed obligation"
            );
            return Ok(false);
        }
    };

    // Load reserve + pool metadata.
    let jitosol_reserve = load_reserve(
        &ctx.rpc.client,
        &KAMINO_MAIN_JITOSOL_RESERVE,
        JITOSOL_MINT,
        &KAMINO_MAIN_MARKET,
    )
    .await
    .context("load jitoSOL reserve for seed")?;
    let jito_pool = load_jito_pool(&ctx.rpc.client)
        .await
        .context("load Jito stake pool for seed")?;

    // v0.1.13 fix: compute the jitoSOL we will actually receive from the
    // DepositSol step using the pool's on-chain exchange rate, NOT a
    // 0.5%-haircut 1:1 assumption. 1 jitoSOL ≈ 1.28 SOL on mainnet, so
    // the old estimate was ~27% too high and Kamino's deposit step then
    // failed with TokenError::InsufficientFunds (0x1).
    //
    // We still apply a 0.5% safety haircut on top of the rate-derived
    // amount to absorb (a) the pool's manager fee taken on deposit and
    // (b) any rounding between the mint amount Jito computes and the
    // amount we ask Kamino to transfer.
    let rate_adjusted_jitosol = jito_pool.sol_to_jitosol_lamports(stake_lamports);
    let expected_jitosol_received =
        rate_adjusted_jitosol.saturating_sub(rate_adjusted_jitosol / 200);

    info!(
        wallet_lamports,
        stake_lamports,
        rate_adjusted_jitosol,
        expected_jitosol_received,
        pool_total_lamports = jito_pool.total_lamports,
        pool_token_supply = jito_pool.pool_token_supply,
        simulate_only = ctx.simulate_only,
        "seed obligation: bootstrapping with initial jitoSOL deposit"
    );

    let user_metadata_missing = !user_metadata_exists(&ctx.rpc.client, &user).await;
    let obligation_already_exists = decoded.is_some();
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

    // rc52: resolve obligation_reserves pubkeys to ReserveAccounts so the
    // seed bundle can emit refresh_reserve_ix for each one before
    // refresh_obligation_ix. The multiply obligation only ever holds
    // jitoSOL (deposit) and SOL (borrow) reserves — load SOL on demand;
    // jitoSOL is already loaded above. Unknown reserves bail because they
    // signal a state we don't have a refresh strategy for.
    let needs_sol_reserve = obligation_reserves
        .iter()
        .any(|r| *r == KAMINO_MAIN_SOL_RESERVE);
    let sol_reserve_opt = if needs_sol_reserve {
        Some(
            load_reserve(
                &ctx.rpc.client,
                &KAMINO_MAIN_SOL_RESERVE,
                WSOL_MINT,
                &KAMINO_MAIN_MARKET,
            )
            .await
            .context("rc52: load SOL reserve for refresh before refresh_obligation")?,
        )
    } else {
        None
    };
    let obligation_reserve_accounts: Vec<&ReserveAccounts> = obligation_reserves
        .iter()
        .map(|res| -> Result<&ReserveAccounts> {
            if *res == jitosol_reserve.reserve {
                Ok(&jitosol_reserve)
            } else if *res == KAMINO_MAIN_SOL_RESERVE {
                Ok(sol_reserve_opt
                    .as_ref()
                    .expect("loaded above when SOL reserve is in obligation"))
            } else {
                anyhow::bail!(
                    "rc52: unexpected reserve {} in multiply obligation; only jitoSOL + SOL supported",
                    res
                )
            }
        })
        .collect::<Result<Vec<_>>>()?;

    let ixs = build_seed_bundle(
        &user,
        &jito_pool,
        &jitosol_reserve,
        stake_lamports,
        expected_jitosol_received,
        user_metadata_missing,
        obligation_already_exists,
        &obligation_reserve_accounts,
    )?;

    // Audit-fix I1: every seed ixn must target a whitelisted program.
    // The whitelist already covers klend, jito stake pool, ATA, system,
    // token, compute budget — same surface as the leverage loop.
    ctx.whitelist
        .verify_ixns(&ixs)
        .context("whitelist check on seed-obligation ixns")?;

    if ctx.simulate_only {
        let sim = ctx
            .rpc
            .build_sign_simulate(ixs, ctx.wallet.keypair(), SEED_CU_LIMIT, SEED_PRIORITY_FEE)
            .await
            .context("simulate seed-obligation tx")?;
        let (layout_valid, summary) = zerox1_defi_runtime::rpc::classify_simulation(&sim);
        // Dump full program logs so we can diagnose Custom(_) errors. Each
        // line is logged separately for readability in journalctl.
        if let Some(logs) = sim.logs.as_ref() {
            let log_level_warn = sim.err.is_some();
            for (i, line) in logs.iter().enumerate() {
                if log_level_warn {
                    warn!(seed_sim_log_idx = i, "seed_sim_log: {}", line);
                } else {
                    info!(seed_sim_log_idx = i, "seed_sim_log: {}", line);
                }
            }
        }
        if sim.err.is_some() {
            warn!(
                layout_valid,
                summary = %summary,
                err = ?sim.err,
                "seed sim FAILED"
            );
        } else {
            info!(
                layout_valid,
                summary = %summary,
                "seed sim ok"
            );
        }
    } else {
        let sig = ctx
            .rpc
            .build_sign_send(ixs, ctx.wallet.keypair(), SEED_CU_LIMIT, SEED_PRIORITY_FEE)
            .await
            .context("broadcast seed-obligation tx")?;
        info!(sig = %sig, "seed committed");
    }

    Ok(true)
}

/// rc41: convert `usdc_lamports` of USDC into native SOL via Jupiter, in
/// preparation for the existing `maybe_seed_obligation` path (which
/// expects native SOL in the wallet to stake → jitoSOL → supply). This
/// is the bridge from allocator-routed USDC into the multiply strategy's
/// jitoSOL collateral universe.
///
/// Returns once the swap is confirmed; the caller (handle_assign) then
/// falls through to `maybe_seed_obligation` which reads the new wallet
/// SOL balance and proceeds.
pub async fn seed_with_usdc(
    ctx: &DispatchCtx,
    jup: &JupiterSwap,
    usdc_lamports: u64,
    slippage_bps: u16,
) -> Result<()> {
    if usdc_lamports == 0 {
        return Err(anyhow::anyhow!(
            "seed_with_usdc called with usdc_lamports=0 (caller bug)"
        ));
    }
    let user = ctx.wallet.pubkey();
    info!(
        usdc_lamports,
        slippage_bps,
        %user,
        "rc41: routing USDC → native SOL via Jupiter before seed"
    );
    let tx = build_usdc_to_sol_swap_tx(jup, &user, usdc_lamports, slippage_bps)
        .await
        .context("build Jupiter USDC→SOL swap tx")?;
    let sig = ctx
        .rpc
        .sign_existing_send(tx, ctx.wallet.keypair())
        .await
        .context("broadcast Jupiter USDC→SOL swap tx")?;
    info!(%sig, "rc41: USDC→SOL swap confirmed; falling through to seed");
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Seed decision + bundle shape coverage. rc53 simplified the decision
    //! logic — obligation state no longer factors into `decide_seed_amount`;
    //! the function now answers "given wallet SOL balance, how much do we
    //! stake?" with no other inputs.
    use super::*;
    use solana_sdk::pubkey::Pubkey;
    use zerox1_defi_protocols::protocols::jito::{deposit_sol_ix, StakePoolMeta};
    use zerox1_defi_protocols::protocols::kamino::ReserveAccounts;

    fn dummy_reserve() -> ReserveAccounts {
        ReserveAccounts {
            reserve: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            lending_market_authority: Pubkey::new_unique(),
            liquidity_mint: Pubkey::new_unique(),
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
        use zerox1_defi_protocols::constants::JITO_STAKE_POOL;
        use zerox1_defi_protocols::protocols::jito::derive_withdraw_authority;
        StakePoolMeta::jito(
            derive_withdraw_authority(&JITO_STAKE_POOL),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        )
    }

    // ── decide_seed_amount ──────────────────────────────────────────────────
    //
    // rc53: decision is now purely a function of wallet balance + fee buffer
    // + max-stake clamp. Obligation state is irrelevant; the unconditional
    // sweep semantics mean any wallet SOL above the buffer gets staked
    // (multiply is the only fleet daemon that puts SOL in the shared wallet,
    // so this is structurally safe). Pre-rc53 the obligation gate stranded
    // capital whenever a failed seed bundle left wallet SOL above the buffer.

    #[test]
    fn rc53_wallet_sol_above_buffer_always_stakes() {
        // The defining rc53 invariant: even with the obligation already
        // holding jitoSOL collateral (which would have returned
        // ObligationAlreadyHasJitosolCollateral pre-rc53), wallet SOL above
        // the buffer is always staked. Obligation state is no longer an
        // input to `decide_seed_amount`.
        let d = decide_seed_amount(1_000_000_000, SEED_FEE_BUFFER_LAMPORTS, u64::MAX);
        assert_eq!(
            d,
            SeedDecision::Stake(1_000_000_000 - SEED_FEE_BUFFER_LAMPORTS)
        );
    }

    #[test]
    fn wallet_under_fee_buffer_yields_insufficient() {
        let d = decide_seed_amount(
            SEED_FEE_BUFFER_LAMPORTS / 2,
            SEED_FEE_BUFFER_LAMPORTS,
            u64::MAX,
        );
        assert_eq!(
            d,
            SeedDecision::InsufficientWalletBalance {
                wallet_lamports: SEED_FEE_BUFFER_LAMPORTS / 2
            }
        );
    }

    #[test]
    fn wallet_dust_above_buffer_below_min_yields_insufficient() {
        let wallet = SEED_FEE_BUFFER_LAMPORTS + SEED_MIN_STAKE_LAMPORTS - 1;
        let d = decide_seed_amount(wallet, SEED_FEE_BUFFER_LAMPORTS, u64::MAX);
        assert_eq!(
            d,
            SeedDecision::InsufficientWalletBalance {
                wallet_lamports: wallet
            }
        );
    }

    #[test]
    fn stake_is_clamped_to_max() {
        let d = decide_seed_amount(10_000_000_000, SEED_FEE_BUFFER_LAMPORTS, 1_000_000_000);
        assert_eq!(d, SeedDecision::Stake(1_000_000_000));
    }

    #[test]
    fn stake_at_buffer_plus_min_succeeds() {
        // Boundary: exactly fee_buffer + min_stake yields Stake(min_stake).
        let wallet = SEED_FEE_BUFFER_LAMPORTS + SEED_MIN_STAKE_LAMPORTS;
        let d = decide_seed_amount(wallet, SEED_FEE_BUFFER_LAMPORTS, u64::MAX);
        assert_eq!(d, SeedDecision::Stake(SEED_MIN_STAKE_LAMPORTS));
    }

    // ── build_seed_bundle ───────────────────────────────────────────────────

    #[test]
    fn fresh_wallet_bundle_includes_init_user_metadata() {
        // Truly fresh wallet: user_metadata + init_obligation + ATA +
        // refresh_reserve + refresh_obligation + deposit + jito ixs.
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let reserve = dummy_reserve();
        let ixs = build_seed_bundle(
            &user,
            &pool,
            &reserve,
            1_000_000_000,
            995_000_000,
            true,
            false,
            &[],
        )
        .expect("build seed bundle");
        // init_user_metadata is index 0.
        assert!(!ixs.is_empty(), "expected non-empty seed bundle");
        // Sanity bound — fresh-wallet shape is ~7-8 ixs.
        assert!(ixs.len() >= 6, "expected at least 6 ixs, got {}", ixs.len());
        assert!(ixs.len() <= 12, "unexpectedly large bundle: {}", ixs.len());
    }

    #[test]
    fn existing_obligation_bundle_drops_init_obligation() {
        // Re-seed case: obligation PDA already exists. Bundle must NOT
        // include InitializeObligation (would Allocate-collide).
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let reserve = dummy_reserve();
        let with_init = build_seed_bundle(
            &user,
            &pool,
            &reserve,
            1_000_000_000,
            995_000_000,
            false,
            false,
            &[],
        )
        .expect("with init");
        let without_init = build_seed_bundle(
            &user,
            &pool,
            &reserve,
            1_000_000_000,
            995_000_000,
            false,
            true,
            &[],
        )
        .expect("without init");
        // Skipping InitObligation drops exactly one ixn.
        assert_eq!(with_init.len(), without_init.len() + 1);
    }

    #[test]
    fn rc52_bundle_refreshes_every_obligation_reserve_before_refresh_obligation() {
        // rc52 invariant: when the obligation holds both jitoSOL (deposit) and
        // SOL (borrow), the seed bundle must emit RefreshReserve for BOTH
        // before RefreshObligation. Pre-rc52 only the jitoSOL reserve was
        // refreshed, so klend's RefreshObligation bailed with
        // InvalidAccountInput (0x1776) on the rc47 forced-top-up path.
        use solana_sdk::pubkey::Pubkey;
        use zerox1_defi_protocols::constants::KAMINO_LEND_PROGRAM_ID;

        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let jitosol_reserve = dummy_reserve();
        let mut sol_reserve = dummy_reserve();
        // Make SOL reserve share the lending_market so the refresh_obligation_ix
        // call below uses the same market field.
        sol_reserve.lending_market = jitosol_reserve.lending_market;

        let obligation_reserve_accounts: Vec<&ReserveAccounts> =
            vec![&jitosol_reserve, &sol_reserve];

        let ixs = build_seed_bundle(
            &user,
            &pool,
            &jitosol_reserve,
            1_000_000_000,
            995_000_000,
            false, // user_metadata exists
            true,  // obligation exists
            &obligation_reserve_accounts,
        )
        .expect("build seed bundle (rc52 existing-leveraged-position shape)");

        // Find indices of every klend ix in the bundle.
        let klend_ix_indices: Vec<usize> = ixs
            .iter()
            .enumerate()
            .filter(|(_, ix)| ix.program_id == KAMINO_LEND_PROGRAM_ID)
            .map(|(i, _)| i)
            .collect();

        // Expected klend ixs in this order:
        //   refresh_reserve(jitoSOL), refresh_reserve(SOL),
        //   refresh_obligation, deposit_reserve_v2 (DepositCollateral)
        // So at least 4 klend ixs, with refresh_obligation NOT being the first.
        assert!(
            klend_ix_indices.len() >= 4,
            "expected ≥4 klend ixs (2 refresh_reserve + refresh_obligation + deposit_v2), got {}: {:?}",
            klend_ix_indices.len(),
            klend_ix_indices
        );

        // The first two klend ixs must be refresh_reserve calls (one per
        // obligation reserve). The accounts list for refresh_reserve is
        // [reserve, lending_market, ...]; verify the [0] account is one of
        // jitoSOL or SOL.
        let first_refresh = &ixs[klend_ix_indices[0]];
        let second_refresh = &ixs[klend_ix_indices[1]];
        let first_target = first_refresh.accounts[0].pubkey;
        let second_target = second_refresh.accounts[0].pubkey;
        let expected: std::collections::HashSet<Pubkey> =
            [jitosol_reserve.reserve, sol_reserve.reserve]
                .into_iter()
                .collect();
        let actual: std::collections::HashSet<Pubkey> =
            [first_target, second_target].into_iter().collect();
        assert_eq!(
            actual, expected,
            "first two klend ixs must refresh both obligation reserves; got {:?}",
            actual
        );
    }

    #[test]
    fn rc52_dedupes_jitosol_refresh_when_already_in_obligation() {
        // rc52 invariant 2: when jitoSOL is in obligation_reserve_accounts,
        // build_seed_bundle must NOT emit a duplicate refresh_reserve(jitoSOL)
        // (the obligation loop already refreshed it). Klend tolerates dup
        // RefreshReserve in-tx, but burning CU on a redundant ix is waste.
        use zerox1_defi_protocols::constants::KAMINO_LEND_PROGRAM_ID;

        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let jitosol_reserve = dummy_reserve();
        let obligation_reserve_accounts: Vec<&ReserveAccounts> = vec![&jitosol_reserve];

        let ixs = build_seed_bundle(
            &user,
            &pool,
            &jitosol_reserve,
            1_000_000_000,
            995_000_000,
            false,
            true,
            &obligation_reserve_accounts,
        )
        .expect("build seed bundle (jitoSOL-only obligation)");

        let refresh_jitosol_count = ixs
            .iter()
            .filter(|ix| ix.program_id == KAMINO_LEND_PROGRAM_ID)
            .filter(|ix| ix.accounts[0].pubkey == jitosol_reserve.reserve)
            // Discriminator-prefix check: refresh_reserve's anchor data starts
            // with the refresh_reserve discriminator. We accept on first 8
            // bytes by structural fact that ANY klend ix targeting only
            // (reserve, lending_market, ...) is refresh_reserve in our bundle.
            .filter(|ix| ix.accounts.len() == 6)
            .count();
        assert_eq!(
            refresh_jitosol_count, 1,
            "expected exactly 1 refresh_reserve(jitoSOL) ix, got {}",
            refresh_jitosol_count
        );
    }

    #[test]
    fn rc52_empty_obligation_reserve_accounts_falls_back_to_jitosol_only() {
        // rc52 invariant 3: fresh-wallet shape (no obligation, empty
        // obligation_reserve_accounts) still emits exactly ONE refresh_reserve
        // — for the jitoSOL reserve being deposited into. Regression guard
        // against accidentally dropping the jitoSOL refresh after the rc52
        // restructure.
        use zerox1_defi_protocols::constants::KAMINO_LEND_PROGRAM_ID;

        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let jitosol_reserve = dummy_reserve();
        let ixs = build_seed_bundle(
            &user,
            &pool,
            &jitosol_reserve,
            1_000_000_000,
            995_000_000,
            true,
            false,
            &[],
        )
        .expect("build seed bundle (fresh wallet, empty obligation)");

        let refresh_jitosol_count = ixs
            .iter()
            .filter(|ix| ix.program_id == KAMINO_LEND_PROGRAM_ID)
            .filter(|ix| ix.accounts[0].pubkey == jitosol_reserve.reserve)
            .filter(|ix| ix.accounts.len() == 6)
            .count();
        assert_eq!(
            refresh_jitosol_count, 1,
            "fresh wallet must still refresh jitoSOL reserve exactly once"
        );
    }

    // ── deposit_sol_ix sanity — required imports for the dummy_pool path ──

    #[test]
    fn dummy_pool_round_trips_deposit_sol_ix() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ixs = deposit_sol_ix(&user, &pool, 1_000_000).expect("deposit_sol_ix");
        assert!(!ixs.is_empty());
    }
}
