//! Kamino USDC supply execution. M6 lands the real ixn set:
//!
//!   1. initialize_obligation_if_missing (idempotent — klend errors are
//!      benign on re-init, and `deposit_ix` always emits the ixn)
//!   2. idempotent ATA create for the user's USDC ATA
//!   3. refresh_reserve
//!   4. deposit_reserve_liquidity_and_obligation_collateral
//!
//! All four are returned by `kamino::deposit_ix` from defi-protocols, so
//! lend.rs just calls that and adds nothing on top — no extra builders
//! were lifted from multiply's leverage.rs (multiply needs them only
//! because rounds 2+ can skip the obligation-init / ATA-create steps;
//! stable-yield always runs round 1, where the bundled `deposit_ix` is
//! exactly what we want).
//!
//! Compute budget ixns (set_compute_unit_limit + set_compute_unit_price)
//! are NOT pushed here — `RpcContext::build_signed` prepends them
//! automatically and the whitelist already covers compute_budget::ID.
//!
//! Audit-fix I1: `SigningWhitelist::verify_ixns` runs before signing on
//! BOTH the sim-only and submit paths. Any ixn whose `program_id` falls
//! outside `kamino::whitelist_program_ids` is rejected before the wallet
//! ever sees the message.

use anyhow::{Context, Result};
use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn};

use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use zerox1_defi_protocols::constants::USDC_MINT;
use zerox1_defi_protocols::protocols::kamino::{
    deposit_ix, derive_lending_market_authority, derive_user_obligation,
    init_obligation_farms_for_reserve_ix, init_user_metadata_ix, withdraw_ix, ReserveAccounts,
};
use zerox1_defi_protocols::protocols::kamino_loader::{
    decode_obligation, load_reserve, obligation_farm_state_exists, user_metadata_exists,
    DecodedObligation, OBLIGATION_DISCRIMINATOR,
};
use zerox1_defi_runtime::rpc::classify_simulation;
use zerox1_protocol::fleet::stable_lend::{
    AssignStableLend, ReportStableLend, ReportStableWithdraw, WithdrawStableLend,
};
use zerox1_protocol::fleet::ReportHeader;

use crate::dispatch::DispatchCtx;
use crate::rates::fetch_kamino_usdc_apr_bps;

/// Single-leg deposit bundle: up to 9 instructions (init_user_metadata +
/// init_obligation + init_farms + ATA-create + refresh_reserve +
/// refresh_obligation + refresh_farms(pre) + deposit + refresh_farms(post)).
/// 600k is ample.
const STABLE_YIELD_CU_LIMIT: u32 = 600_000;
const STABLE_YIELD_PRIORITY_FEE: u64 = 10_000;

/// Error code emitted when build_sign_simulate returns a TransactionError.
/// Distinct from cap (3) and inner-failure (1, 2) codes used by dispatch.rs.
const ERROR_CODE_SIM_FAILED: u32 = 5;
/// Error code emitted when reserve loading or ixn-building blows up before
/// we even reach the simulate/submit step. Same surface (anyhow → Report)
/// as ERROR_CODE_SIM_FAILED but a distinct code so operators can grep.
const ERROR_CODE_BUILD_FAILED: u32 = 6;

/// Build the four-ixn Kamino USDC supply bundle, run it through
/// `SigningWhitelist::verify_ixns`, then either simulate it (sim-only mode)
/// or broadcast it.
///
/// All anyhow errors raised on the build path are converted to error-coded
/// Reports rather than bubbling — the dispatch loop can still emit a
/// well-formed Report to the orchestrator and the daemon stays alive.
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    payload: &AssignStableLend,
    conv: [u8; 16],
) -> Result<ReportStableLend> {
    let payer = ctx.wallet.pubkey();
    let market = Pubkey::new_from_array(payload.market);
    let reserve_pubkey = Pubkey::new_from_array(payload.reserve);

    info!(
        ?conv,
        usdc_lamports = payload.usdc_lamports,
        market = %market,
        reserve = %reserve_pubkey,
        simulate_only = ctx.simulate_only,
        "stable-yield deposit starting"
    );

    // Build phase — pull the on-chain reserve metadata and derive the ixn set.
    // We catch any anyhow error here and convert it to a build-failed Report so
    // a missing reserve (e.g. devnet placeholder pubkey) doesn't crash the
    // daemon.
    let ixs =
        match build_supply_ixns(ctx, payer, market, reserve_pubkey, payload.usdc_lamports).await {
            Ok(v) => v,
            Err(e) => {
                warn!(?conv, ?e, "supply ixn build failed");
                return Ok(ReportStableLend {
                    header: ReportHeader::err(conv, ERROR_CODE_BUILD_FAILED),
                    deposited_usdc_lamports: 0,
                    current_apr_bps: 0,
                    tx_signature: None,
                });
            }
        };

    // Audit-fix I1: structural authority boundary. Every ixn in the bundle
    // must target a program in the daemon's signing whitelist. RpcContext
    // additionally prepends two compute-budget ixns, which are also covered
    // by the whitelist (compute_budget::ID).
    ctx.whitelist
        .verify_ixns(&ixs)
        .context("whitelist check on stable-yield deposit ixns")?;
    info!(?conv, ix_count = ixs.len(), "whitelist check passed");

    if ctx.simulate_only {
        info!(?conv, "simulate_only=true — running build_sign_simulate");
        match ctx
            .rpc
            .build_sign_simulate(
                ixs,
                ctx.wallet.keypair(),
                STABLE_YIELD_CU_LIMIT,
                STABLE_YIELD_PRIORITY_FEE,
            )
            .await
        {
            Ok(sim) => {
                let (layout_valid, summary) = classify_simulation(&sim);
                if sim.err.is_some() {
                    let logs = sim.logs.as_deref().unwrap_or(&[]).join(" | ");
                    warn!(
                        ?conv,
                        layout_valid,
                        summary = %summary,
                        program_logs = %logs,
                        "simulation returned error (expected on devnet w/ placeholder reserve)"
                    );
                    return Ok(ReportStableLend {
                        header: ReportHeader::err(conv, ERROR_CODE_SIM_FAILED),
                        deposited_usdc_lamports: 0,
                        current_apr_bps: 0,
                        tx_signature: None,
                    });
                }
                let apr_bps = fetch_kamino_usdc_apr_bps().await;
                info!(?conv, layout_valid, summary = %summary, apr_bps, "simulation succeeded");
                Ok(ReportStableLend {
                    header: ReportHeader::ok(conv),
                    deposited_usdc_lamports: payload.usdc_lamports,
                    current_apr_bps: apr_bps,
                    tx_signature: None,
                })
            }
            Err(e) => {
                warn!(?conv, ?e, "build_sign_simulate threw");
                Ok(ReportStableLend {
                    header: ReportHeader::err(conv, ERROR_CODE_SIM_FAILED),
                    deposited_usdc_lamports: 0,
                    current_apr_bps: 0,
                    tx_signature: None,
                })
            }
        }
    } else {
        info!(?conv, "submit path — broadcasting deposit");
        match ctx
            .rpc
            .build_sign_send(
                ixs,
                ctx.wallet.keypair(),
                STABLE_YIELD_CU_LIMIT,
                STABLE_YIELD_PRIORITY_FEE,
            )
            .await
        {
            Ok(sig) => {
                let apr_bps = fetch_kamino_usdc_apr_bps().await;
                info!(?conv, %sig, apr_bps, "deposit confirmed on-chain");
                Ok(ReportStableLend {
                    header: ReportHeader::ok(conv),
                    deposited_usdc_lamports: payload.usdc_lamports,
                    current_apr_bps: apr_bps,
                    tx_signature: Some(sig.to_string()),
                })
            }
            Err(e) => {
                warn!(?conv, ?e, "build_sign_send failed");
                Ok(ReportStableLend {
                    header: ReportHeader::err(conv, ERROR_CODE_SIM_FAILED),
                    deposited_usdc_lamports: 0,
                    current_apr_bps: 0,
                    tx_signature: None,
                })
            }
        }
    }
}

/// Pull the reserve metadata from chain (with `load_reserve`) and build the
/// four-ixn USDC supply bundle. Falls back to a synthetic `ReserveAccounts`
/// built off `derive_lending_market_authority` + the canonical USDC mint
/// when the chain account does not exist or has the wrong owner — that
/// path keeps devnet smoke meaningful (we still get the same ixn shape, the
/// chain just rejects it during simulation, which is what the M6 verification
/// expects).
async fn build_supply_ixns(
    ctx: &DispatchCtx,
    user: Pubkey,
    market: Pubkey,
    reserve_pubkey: Pubkey,
    amount_lamports: u64,
) -> Result<Vec<solana_sdk::instruction::Instruction>> {
    if amount_lamports == 0 {
        anyhow::bail!("usdc_lamports must be > 0");
    }

    // Try the live-reserve path first. Fail open to a placeholder layout when
    // the reserve doesn't exist (devnet smoke), so verify_ixns still gets to
    // run and the simulation still surfaces a real error.
    let reserve = match load_reserve(&ctx.rpc.client, &reserve_pubkey, USDC_MINT, &market).await {
        Ok(r) => {
            info!(reserve = %reserve_pubkey, "loaded live Kamino reserve metadata");
            r
        }
        Err(e) => {
            warn!(
                reserve = %reserve_pubkey,
                ?e,
                "load_reserve failed (likely placeholder pubkey on devnet); \
                 falling back to synthetic ReserveAccounts so the wiring \
                 is still exercised"
            );
            ReserveAccounts {
                reserve: reserve_pubkey,
                lending_market: market,
                lending_market_authority: derive_lending_market_authority(&market),
                liquidity_mint: USDC_MINT,
                liquidity_supply: reserve_pubkey, // bogus — sim will reject
                fee_receiver: reserve_pubkey,
                collateral_mint: reserve_pubkey,
                collateral_supply: reserve_pubkey,
                scope_prices: Pubkey::default(),
                farm_collateral: Pubkey::default(),
            }
        }
    };

    // Bug fix (2026-05-13): a second deposit to the same reserve from the same
    // wallet hits `Allocate: account already in use` because `deposit_ix`
    // unconditionally prepends `InitializeObligation`, which `system_program::
    // Allocate`s the obligation PDA. Fetch the obligation account and drop the
    // InitObligation ixn when the PDA already has data.
    //
    // Bug fix (2026-05-13, follow-up): once the obligation has registered
    // reserves, klend's RefreshObligation requires each as a remaining account
    // in array order (deposits, then borrows). Without them: InvalidAccountInput
    // (0x1776). Parse the obligation and pass the list through deposit_ix.
    let obligation = derive_user_obligation(&user, &market);
    let (obligation_already_exists, obligation_reserves) =
        fetch_obligation_reserves(&ctx.rpc.client, &obligation).await;

    // Build the core deposit instruction bundle (initialize_obligation + ATA +
    // refresh_farms + refresh_obligation + refresh_reserve + deposit). The
    // RefreshObligation ixn carries the obligation's registered reserves as
    // remaining accounts.
    // Stable-yield always uses the (0, 0) seed — its existing on-chain
    // obligation (e.g. BPEv2HG... on mainnet) was derived with these bytes.
    let mut ixs = deposit_ix(
        &user,
        &reserve,
        amount_lamports,
        (0, 0),
        &obligation_reserves,
    )
    .context("build deposit_ix")?;

    if obligation_already_exists {
        // ixs[0] is the InitObligation ixn — see `kamino::deposit_ix`.
        info!(
            %obligation,
            "obligation account already exists; dropping InitializeObligation ixn"
        );
        ixs.remove(0);
    }

    // Track insertion offset: instructions prepended before initialize_obligation
    // shift the index of everything after them.
    let mut prefix_count: usize = 0;

    // For a fresh wallet, user_metadata must be initialized before
    // initialize_obligation can succeed. Prepend at position 0. Skip entirely
    // when the obligation already exists — user_metadata is a prerequisite of
    // initialize_obligation, so its presence is implied.
    if !obligation_already_exists && !user_metadata_exists(&ctx.rpc.client, &user).await {
        info!("user_metadata not found — prepending init_user_metadata_ix");
        ixs.insert(0, init_user_metadata_ix(&user));
        prefix_count += 1;
    }

    // If the reserve has a collateral farm, the obligationFarmUserState must be
    // initialized (once) before RefreshObligationFarmsForReserve can run.
    // It must go AFTER initialize_obligation (index prefix_count) so the
    // obligation account exists when the farms init touches it. When we've
    // skipped InitObligation, the obligation already exists, so the farm-init
    // ixn can go at the front of the bundle (position 0).
    if reserve.farm_collateral != Pubkey::default()
        && !obligation_farm_state_exists(&ctx.rpc.client, &reserve.farm_collateral, &user, &market)
            .await
    {
        info!("obligationFarmUserState not found — inserting init_obligation_farms_for_reserve_ix");
        let insert_at = if obligation_already_exists {
            0
        } else {
            // Insert after initialize_obligation_ix (which is at prefix_count).
            prefix_count + 1
        };
        ixs.insert(
            insert_at,
            init_obligation_farms_for_reserve_ix(&user, &user, &reserve, (0, 0)),
        );
    }

    Ok(ixs)
}

/// Fetch the obligation account and extract:
///   * whether the caller should skip `InitializeObligation` (account already
///     has data on chain), and
///   * the ordered list of registered reserve pubkeys to pass as
///     RefreshObligation remaining accounts (deposits first, then borrows, in
///     on-chain array order, skipping zeroed slots).
///
/// Both supply and withdraw paths use this. When the obligation doesn't exist
/// yet (fresh wallet) or RPC errors, returns `(false, vec![])` — the deposit
/// will run InitObligation and RefreshObligation sees an empty obligation.
async fn fetch_obligation_reserves(rpc: &RpcClient, obligation: &Pubkey) -> (bool, Vec<Pubkey>) {
    let data = match rpc.get_account_data(obligation).await {
        Ok(data) => data,
        Err(e) => {
            info!(
                %obligation, ?e,
                "obligation account not found or RPC error; keeping InitObligation, zero remaining reserves"
            );
            return (false, Vec::new());
        }
    };
    let skip_init = decide_skip_init_obligation(obligation, &data);
    let reserves = decode_obligation_reserves(*obligation, &data);
    (skip_init, reserves)
}

/// Pure helper: given the raw obligation account data, return the ordered
/// reserve list to pass as RefreshObligation remaining accounts. Returns an
/// empty Vec when the data is too short / wrong discriminator / decode fails
/// — callers treat that as "no remaining accounts required" (which klend
/// accepts only for a fresh obligation; on a corrupt obligation the deposit
/// will fail at klend with a clearer error than ours would be).
fn decode_obligation_reserves(address: Pubkey, data: &[u8]) -> Vec<Pubkey> {
    match decode_obligation(address, data) {
        Ok(DecodedObligation {
            deposits, borrows, ..
        }) => deposits
            .into_iter()
            .map(|d| d.reserve)
            .chain(borrows.into_iter().map(|b| b.reserve))
            .collect(),
        Err(e) => {
            warn!(
                %address, ?e,
                "decode_obligation failed; passing empty remaining_accounts to RefreshObligation"
            );
            Vec::new()
        }
    }
}

/// Pure decision: given the raw account data for the obligation PDA, return
/// whether the caller should skip the InitObligation ixn. Factored out for
/// unit testing (no RPC needed).
fn decide_skip_init_obligation(obligation: &Pubkey, data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    if data.len() < 8 || data[..8] != OBLIGATION_DISCRIMINATOR {
        warn!(
            %obligation,
            data_len = data.len(),
            "obligation account exists with non-Kamino-Obligation discriminator; \
             skipping InitObligation anyway — klend will surface a clearer error"
        );
    }
    true
}

/// Build the three-ixn Kamino USDC withdraw bundle (idempotent ATA-create
/// + refresh_reserve + withdraw_obligation_collateral_and_redeem_reserve_collateral),
/// run it through `SigningWhitelist::verify_ixns`, then either simulate it
/// (sim-only mode) or broadcast it.
///
/// Symmetric to `run_or_simulate` for the deposit path. Same anyhow→Report
/// conversion semantics: build failures map to error_code=6, sim/submit
/// failures map to error_code=5.
///
/// Special amount: `u64::MAX` instructs klend to withdraw the obligation's
/// full collateral. The caller is expected to have validated that
/// `payload.usdc_lamports != 0` already (see `caps::validate_withdraw`),
/// but we re-check here as defense in depth.
pub async fn run_withdraw_or_simulate(
    ctx: &DispatchCtx,
    payload: &WithdrawStableLend,
    conv: [u8; 16],
) -> Result<ReportStableWithdraw> {
    let payer = ctx.wallet.pubkey();
    let market = Pubkey::new_from_array(payload.market);
    let reserve_pubkey = Pubkey::new_from_array(payload.reserve);

    info!(
        ?conv,
        usdc_lamports = payload.usdc_lamports,
        market = %market,
        reserve = %reserve_pubkey,
        simulate_only = ctx.simulate_only,
        full_withdraw = (payload.usdc_lamports == u64::MAX),
        "stable-yield withdraw starting"
    );

    let ixs = match build_withdraw_ixns(ctx, payer, market, reserve_pubkey, payload.usdc_lamports)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(?conv, ?e, "withdraw ixn build failed");
            return Ok(ReportStableWithdraw {
                header: ReportHeader::err(conv, ERROR_CODE_BUILD_FAILED),
                withdrawn_usdc_lamports: 0,
                tx_signature: None,
            });
        }
    };

    // Audit-fix I1: same whitelist boundary as the deposit path.
    ctx.whitelist
        .verify_ixns(&ixs)
        .context("whitelist check on stable-yield withdraw ixns")?;
    info!(
        ?conv,
        ix_count = ixs.len(),
        "withdraw whitelist check passed"
    );

    if ctx.simulate_only {
        info!(
            ?conv,
            "simulate_only=true — running build_sign_simulate (withdraw)"
        );
        match ctx
            .rpc
            .build_sign_simulate(
                ixs,
                ctx.wallet.keypair(),
                STABLE_YIELD_CU_LIMIT,
                STABLE_YIELD_PRIORITY_FEE,
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
                        "withdraw simulation returned error \
                         (expected on devnet w/ placeholder reserve)"
                    );
                    return Ok(ReportStableWithdraw {
                        header: ReportHeader::err(conv, ERROR_CODE_SIM_FAILED),
                        withdrawn_usdc_lamports: 0,
                        tx_signature: None,
                    });
                }
                info!(?conv, layout_valid, summary = %summary, "withdraw simulation succeeded");
                Ok(ReportStableWithdraw {
                    header: ReportHeader::ok(conv),
                    // Sim path can't observe the actual amount (the
                    // obligation's deposited_amount is what klend pulls
                    // for u64::MAX). Echo the requested amount; on full
                    // withdraw the caller can disambiguate via the sentinel.
                    withdrawn_usdc_lamports: payload.usdc_lamports,
                    tx_signature: None,
                })
            }
            Err(e) => {
                warn!(?conv, ?e, "withdraw build_sign_simulate threw");
                Ok(ReportStableWithdraw {
                    header: ReportHeader::err(conv, ERROR_CODE_SIM_FAILED),
                    withdrawn_usdc_lamports: 0,
                    tx_signature: None,
                })
            }
        }
    } else {
        info!(?conv, "submit path — broadcasting withdraw");
        match ctx
            .rpc
            .build_sign_send(
                ixs,
                ctx.wallet.keypair(),
                STABLE_YIELD_CU_LIMIT,
                STABLE_YIELD_PRIORITY_FEE,
            )
            .await
        {
            Ok(sig) => {
                info!(?conv, %sig, "withdraw confirmed on-chain");
                Ok(ReportStableWithdraw {
                    header: ReportHeader::ok(conv),
                    withdrawn_usdc_lamports: payload.usdc_lamports,
                    tx_signature: Some(sig.to_string()),
                })
            }
            Err(e) => {
                warn!(?conv, ?e, "withdraw build_sign_send failed");
                Ok(ReportStableWithdraw {
                    header: ReportHeader::err(conv, ERROR_CODE_SIM_FAILED),
                    withdrawn_usdc_lamports: 0,
                    tx_signature: None,
                })
            }
        }
    }
}

/// Pull the reserve metadata and build the withdraw ixn bundle. Mirrors
/// `build_supply_ixns`: live-load attempted first, falls back to a
/// synthetic ReserveAccounts so the wiring stays exercised on devnet
/// placeholder pubkeys (sim still rejects, which is the intended shape
/// of the smoke test).
async fn build_withdraw_ixns(
    ctx: &DispatchCtx,
    user: Pubkey,
    market: Pubkey,
    reserve_pubkey: Pubkey,
    amount_lamports: u64,
) -> Result<Vec<solana_sdk::instruction::Instruction>> {
    if amount_lamports == 0 {
        anyhow::bail!("usdc_lamports must be > 0 (or u64::MAX for full withdraw)");
    }

    let reserve = match load_reserve(&ctx.rpc.client, &reserve_pubkey, USDC_MINT, &market).await {
        Ok(r) => {
            info!(reserve = %reserve_pubkey, "loaded live Kamino reserve metadata (withdraw)");
            r
        }
        Err(e) => {
            warn!(
                reserve = %reserve_pubkey,
                ?e,
                "load_reserve failed for withdraw (likely placeholder pubkey on devnet); \
                 falling back to synthetic ReserveAccounts"
            );
            ReserveAccounts {
                reserve: reserve_pubkey,
                lending_market: market,
                lending_market_authority: derive_lending_market_authority(&market),
                liquidity_mint: USDC_MINT,
                liquidity_supply: reserve_pubkey, // bogus — sim will reject
                fee_receiver: reserve_pubkey,
                collateral_mint: reserve_pubkey,
                collateral_supply: reserve_pubkey,
                scope_prices: Pubkey::default(),
                farm_collateral: Pubkey::default(),
            }
        }
    };

    // Bug fix (2026-05-13): RefreshObligation (now included in the withdraw
    // bundle) requires the obligation's registered reserves as remaining
    // accounts in array order — same as the deposit path.
    let obligation = derive_user_obligation(&user, &market);
    let (_, obligation_reserves) = fetch_obligation_reserves(&ctx.rpc.client, &obligation).await;

    let ixs = withdraw_ix(
        &user,
        &reserve,
        amount_lamports,
        (0, 0),
        &obligation_reserves,
    )
    .context("build withdraw_ix")?;
    Ok(ixs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cu_limit_sane() {
        // klend deposit + obligation init + ATA + refresh + farms (pre+post).
        assert!(STABLE_YIELD_CU_LIMIT >= 200_000);
        assert!(STABLE_YIELD_CU_LIMIT <= 1_400_000);
    }

    #[test]
    fn error_codes_distinct() {
        assert_ne!(ERROR_CODE_SIM_FAILED, ERROR_CODE_BUILD_FAILED);
    }

    #[test]
    fn skip_init_when_obligation_already_exists() {
        // Simulate a fully-formed obligation account: starts with the Kamino
        // Anchor discriminator, plus arbitrary tail bytes. decide_skip should
        // return true (skip InitObligation) — this is the second-deposit case
        // that was failing on mainnet with `Allocate: already in use`.
        let mut data = OBLIGATION_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&[0u8; 64]); // fake remaining state
        assert!(decide_skip_init_obligation(&Pubkey::new_unique(), &data));
    }

    #[test]
    fn keep_init_when_obligation_account_is_empty() {
        // Empty data == account does not exist (or was just rent-collected).
        // First deposit must run InitObligation.
        assert!(!decide_skip_init_obligation(&Pubkey::new_unique(), &[]));
    }

    #[test]
    fn skip_init_with_warn_on_unexpected_discriminator() {
        // Account exists at the PDA but with a different program's data
        // (rare: somebody else initialised it). Per spec we still skip
        // InitObligation — the deposit will fail at the klend level rather
        // than at the system_program Allocate boundary, with a clearer error.
        let data = vec![0xAA; 64];
        assert!(decide_skip_init_obligation(&Pubkey::new_unique(), &data));
    }

    // ── decode_obligation_reserves tests ────────────────────────────────────
    //
    // These mirror the on-chain Obligation layout (see kamino_loader.rs):
    //   [  0..  8] discriminator
    //   [ 32.. 64] lending_market
    //   [ 64.. 96] owner
    //   [ 96..1184] deposits: [ObligationCollateral; 8]  (136B each)
    //               slot[i].reserve at  +0..+32
    //               slot[i].deposited_amount  +32..+40
    //               slot[i].market_value_sf   +40..+56
    //   [1208..2128] borrows: [ObligationLiquidity; 5]   (184B each)
    //               slot[i].reserve at  +0..+32
    //               slot[i].borrowed_amount_sf  +56..+72
    //               slot[i].market_value_sf     +72..+88
    //               slot[i].bfa_market_value_sf +88..+104
    //   total min size = OBLIGATION_AGGREGATE_OFFSET (2128) + 16*4 = 2192
    const OBLIG_LM_OFF: usize = 32;
    const OBLIG_OWNER_OFF: usize = 64;
    const OBLIG_DEPOSITS_OFF: usize = 96;
    const OBLIG_DEPOSIT_STRIDE: usize = 136;
    const OBLIG_BORROWS_OFF: usize = 1208;
    const OBLIG_BORROW_STRIDE: usize = 184;
    const OBLIG_MIN_SIZE: usize = 2128 + 16 * 4;

    fn make_obligation(deposit_reserves: &[Pubkey], active_borrows: &[Pubkey]) -> Vec<u8> {
        let mut buf = vec![0u8; OBLIG_MIN_SIZE];
        buf[0..8].copy_from_slice(&OBLIGATION_DISCRIMINATOR);
        let market = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        buf[OBLIG_LM_OFF..OBLIG_LM_OFF + 32].copy_from_slice(&market.to_bytes());
        buf[OBLIG_OWNER_OFF..OBLIG_OWNER_OFF + 32].copy_from_slice(&owner.to_bytes());
        for (i, r) in deposit_reserves.iter().enumerate() {
            let off = OBLIG_DEPOSITS_OFF + i * OBLIG_DEPOSIT_STRIDE;
            buf[off..off + 32].copy_from_slice(&r.to_bytes());
            // non-zero deposited_amount so the slot is unambiguously "active"
            buf[off + 32..off + 40].copy_from_slice(&1_u64.to_le_bytes());
        }
        for (i, r) in active_borrows.iter().enumerate() {
            let off = OBLIG_BORROWS_OFF + i * OBLIG_BORROW_STRIDE;
            buf[off..off + 32].copy_from_slice(&r.to_bytes());
            // non-zero borrowed_amount_sf so decode_obligation keeps the slot
            // (decode_obligation filters closed borrows by borrowed_amount_sf==0)
            buf[off + 56..off + 72].copy_from_slice(&1_u128.to_le_bytes());
        }
        buf
    }

    #[test]
    fn reserves_for_obligation_with_one_deposit_no_borrows() {
        // Mainnet bug case: prior $5 USDC deposit registered the USDC reserve
        // on the obligation. Next deposit / withdraw must pass that reserve
        // as the single remaining account on RefreshObligation.
        let usdc_reserve = Pubkey::new_unique();
        let buf = make_obligation(&[usdc_reserve], &[]);
        let reserves = decode_obligation_reserves(Pubkey::new_unique(), &buf);
        assert_eq!(reserves, vec![usdc_reserve]);
    }

    #[test]
    fn reserves_for_fresh_obligation_is_empty() {
        // Just-initialized obligation (post-init, no collateral yet). Empty
        // deposits + empty borrows → zero remaining accounts. Pre-v0.1.5
        // behavior preserved for this case.
        let buf = make_obligation(&[], &[]);
        let reserves = decode_obligation_reserves(Pubkey::new_unique(), &buf);
        assert!(reserves.is_empty());
    }

    #[test]
    fn reserves_preserve_order_deposits_then_borrows() {
        // 2 deposits + 1 borrow → 3 remaining accounts in deposit-then-borrow
        // order. klend validates positionally against deposits[]++borrows[].
        let d0 = Pubkey::new_unique();
        let d1 = Pubkey::new_unique();
        let b0 = Pubkey::new_unique();
        let buf = make_obligation(&[d0, d1], &[b0]);
        let reserves = decode_obligation_reserves(Pubkey::new_unique(), &buf);
        assert_eq!(reserves, vec![d0, d1, b0]);
    }

    #[test]
    fn reserves_for_garbage_data_is_empty() {
        // Account exists with garbage / wrong-discriminator: decode fails,
        // we fall back to an empty list. The deposit then fails at klend
        // with a clearer error than we'd produce.
        let reserves = decode_obligation_reserves(Pubkey::new_unique(), &[0xAA; 64]);
        assert!(reserves.is_empty());
    }
}
