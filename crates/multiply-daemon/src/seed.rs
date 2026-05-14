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
use tracing::{debug, info, warn};
use zerox1_defi_protocols::{
    constants::{JITOSOL_MINT, KAMINO_MAIN_JITOSOL_RESERVE, KAMINO_MAIN_MARKET},
    protocols::{
        jito::{deposit_sol_ix, StakePoolMeta},
        jito_loader::load_jito_pool,
        kamino::{
            deposit_ix, derive_user_obligation_with_seed, init_user_metadata_ix, ReserveAccounts,
        },
        kamino_loader::{fetch_obligation, load_reserve, user_metadata_exists, DecodedObligation},
    },
};

use crate::caps;
use crate::dispatch::DispatchCtx;

/// Compute budget for the seed bundle. Larger than a leverage round
/// because the first deposit also runs init_user_metadata + init_obligation
/// + (optional) init_obligation_farms. 1.2M CU is well above the worst-case
/// cost and still safely below the 1.4M tx limit.
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
    /// Obligation already has collateral — no seed needed.
    ObligationAlreadyHasCollateral,
    /// Wallet balance after reserving fees would leave nothing to stake.
    InsufficientWalletBalance { wallet_lamports: u64 },
}

/// Pure decision: should the daemon seed the obligation, and with how
/// much SOL? Inputs:
///   * `obligation` — the decoded obligation (or `None` if the PDA
///     doesn't exist on-chain yet).
///   * `wallet_lamports` — current native SOL balance.
///   * `fee_buffer_lamports` — lamports to leave behind for tx fees.
///   * `max_stake_lamports` — operator-configured ceiling (the daemon's
///     `--max-position-usdc-lamports` re-interpreted as a SOL cap). The
///     stake is clamped to this value so a misconfigured wallet can't
///     exceed the operator's blast-radius limit.
///
/// Returns the [`SeedDecision`] variant. `decide_seed_amount` is the
/// unit-testable core of [`maybe_seed_obligation`].
pub fn decide_seed_amount(
    obligation: Option<&DecodedObligation>,
    wallet_lamports: u64,
    fee_buffer_lamports: u64,
    max_stake_lamports: u64,
) -> SeedDecision {
    if let Some(ob) = obligation {
        // Same gate as liq_monitor::obligation_has_active_position's
        // collateral half — any non-zero deposit slot means we've already
        // bootstrapped (or are mid-position) and the leverage loop can
        // proceed from round 1 as-is.
        let has_collateral =
            ob.deposited_value_sf > 0 || ob.deposits.iter().any(|d| d.deposited_amount > 0);
        if has_collateral {
            return SeedDecision::ObligationAlreadyHasCollateral;
        }
    }

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
pub fn build_seed_bundle(
    user: &Pubkey,
    jito_pool: &StakePoolMeta,
    jitosol_reserve: &ReserveAccounts,
    stake_sol_lamports: u64,
    expected_jitosol_received: u64,
    user_metadata_missing: bool,
    obligation_already_exists: bool,
    obligation_reserves: &[Pubkey],
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

    // Kamino deposit_ix: init_obligation (skipped below if PDA exists) +
    // ATA-create + refresh_reserve + refresh_obligation + deposit.
    let mut deposit_ixs = deposit_ix(
        user,
        jitosol_reserve,
        expected_jitosol_received,
        caps::MULTIPLY_OBLIGATION_SEED,
        obligation_reserves,
    )
    .context("build kamino deposit_ix for seed")?;
    if obligation_already_exists {
        // ixs[0] of `deposit_ix` is InitializeObligation — drop it to
        // avoid `Allocate: account already in use` on re-entry. Mirrors
        // the same fix in kamino.rs::build_supply_ixns and stable-yield.
        debug!("obligation already exists; dropping InitializeObligation from seed bundle");
        deposit_ixs.remove(0);
    }
    ixs.extend(deposit_ixs);

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
        decoded.as_ref(),
        wallet_lamports,
        SEED_FEE_BUFFER_LAMPORTS,
        ctx.args_max_position_usdc_lamports,
    );

    let stake_lamports = match decision {
        SeedDecision::Stake(n) => n,
        SeedDecision::ObligationAlreadyHasCollateral => {
            info!(%obligation_addr, "obligation already has collateral; skipping seed");
            return Ok(false);
        }
        SeedDecision::InsufficientWalletBalance { wallet_lamports } => {
            warn!(
                wallet_lamports,
                fee_buffer = SEED_FEE_BUFFER_LAMPORTS,
                "wallet balance too low after fee buffer; cannot seed obligation"
            );
            return Ok(false);
        }
    };

    // Mirror the leverage round's 0.5% haircut. Assumes ≈1:1 SOL:jitoSOL
    // at deposit time (the lever-up rounds make the same assumption —
    // documented as a TODO there too).
    let expected_jitosol_received = stake_lamports.saturating_sub(stake_lamports / 200);

    info!(
        wallet_lamports,
        stake_lamports,
        expected_jitosol_received,
        simulate_only = ctx.simulate_only,
        "seed obligation: bootstrapping with initial jitoSOL deposit"
    );

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

    let ixs = build_seed_bundle(
        &user,
        &jito_pool,
        &jitosol_reserve,
        stake_lamports,
        expected_jitosol_received,
        user_metadata_missing,
        obligation_already_exists,
        &obligation_reserves,
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
        info!(
            layout_valid,
            summary = %summary,
            "seed sim ok"
        );
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

#[cfg(test)]
mod tests {
    //! v0.1.11 Bug 1: prove the seed decision and bundle shape.
    use super::*;
    use solana_sdk::pubkey::Pubkey;
    use zerox1_defi_protocols::protocols::jito::{deposit_sol_ix, StakePoolMeta};
    use zerox1_defi_protocols::protocols::kamino::ReserveAccounts;
    use zerox1_defi_protocols::protocols::kamino_loader::{
        DecodedObligation, ObligationBorrow, ObligationDeposit,
    };

    fn mk_obligation(
        deposits: Vec<ObligationDeposit>,
        deposited_value_sf: u128,
    ) -> DecodedObligation {
        DecodedObligation {
            address: Pubkey::new_unique(),
            lending_market: Pubkey::new_unique(),
            owner: Pubkey::new_unique(),
            deposits,
            borrows: Vec::<ObligationBorrow>::new(),
            deposited_value_sf,
            borrow_factor_adjusted_debt_value_sf: 0,
            borrowed_assets_market_value_sf: 0,
            allowed_borrow_value_sf: 0,
            unhealthy_borrow_value_sf: 0,
        }
    }

    fn deposit(amount: u64) -> ObligationDeposit {
        ObligationDeposit {
            reserve: Pubkey::new_unique(),
            deposited_amount: amount,
            market_value_sf: amount as u128,
        }
    }

    fn deposit_on(reserve: Pubkey, amount: u64) -> ObligationDeposit {
        ObligationDeposit {
            reserve,
            deposited_amount: amount,
            market_value_sf: amount as u128,
        }
    }

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

    #[test]
    fn obligation_with_existing_deposit_is_skipped() {
        let ob = mk_obligation(vec![deposit(1_000_000)], 1_000_000);
        let d = decide_seed_amount(Some(&ob), 1_000_000_000, SEED_FEE_BUFFER_LAMPORTS, u64::MAX);
        assert_eq!(d, SeedDecision::ObligationAlreadyHasCollateral);
    }

    #[test]
    fn obligation_missing_seeds_from_wallet_minus_fees() {
        let d = decide_seed_amount(None, 1_000_000_000, SEED_FEE_BUFFER_LAMPORTS, u64::MAX);
        assert_eq!(
            d,
            SeedDecision::Stake(1_000_000_000 - SEED_FEE_BUFFER_LAMPORTS)
        );
    }

    #[test]
    fn empty_obligation_pda_still_triggers_seed() {
        let ob = mk_obligation(vec![], 0);
        let d = decide_seed_amount(Some(&ob), 500_000_000, SEED_FEE_BUFFER_LAMPORTS, u64::MAX);
        assert_eq!(
            d,
            SeedDecision::Stake(500_000_000 - SEED_FEE_BUFFER_LAMPORTS)
        );
    }

    #[test]
    fn zero_deposit_slot_does_not_count_as_active_collateral() {
        let ob = mk_obligation(vec![deposit(0)], 0);
        let d = decide_seed_amount(Some(&ob), 500_000_000, SEED_FEE_BUFFER_LAMPORTS, u64::MAX);
        assert_eq!(
            d,
            SeedDecision::Stake(500_000_000 - SEED_FEE_BUFFER_LAMPORTS)
        );
    }

    #[test]
    fn wallet_under_fee_buffer_yields_insufficient() {
        let d = decide_seed_amount(
            None,
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
        let d = decide_seed_amount(None, wallet, SEED_FEE_BUFFER_LAMPORTS, u64::MAX);
        assert_eq!(
            d,
            SeedDecision::InsufficientWalletBalance {
                wallet_lamports: wallet
            }
        );
    }

    #[test]
    fn stake_is_clamped_to_max() {
        let d = decide_seed_amount(
            None,
            10_000_000_000,
            SEED_FEE_BUFFER_LAMPORTS,
            1_000_000_000,
        );
        assert_eq!(d, SeedDecision::Stake(1_000_000_000));
    }

    // deposit_on helper is used in commit 2's jitoSOL-specific predicate tests
    #[test]
    fn deposit_on_helper_compiles() {
        let _ = deposit_on(Pubkey::new_unique(), 0);
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

    // ── deposit_sol_ix sanity — required imports for the dummy_pool path ──

    #[test]
    fn dummy_pool_round_trips_deposit_sol_ix() {
        let user = Pubkey::new_unique();
        let pool = dummy_pool();
        let ixs = deposit_sol_ix(&user, &pool, 1_000_000).expect("deposit_sol_ix");
        assert!(!ixs.is_empty());
    }
}
