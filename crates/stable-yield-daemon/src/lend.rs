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

use zerox1_defi_protocols::constants::USDC_MINT;
use zerox1_defi_protocols::protocols::kamino::{
    deposit_ix, derive_lending_market_authority, init_obligation_farms_for_reserve_ix,
    init_user_metadata_ix, withdraw_ix, ReserveAccounts,
};
use zerox1_defi_protocols::protocols::kamino_loader::{
    load_reserve, obligation_farm_state_exists, user_metadata_exists,
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

    // Build the core deposit instruction bundle (initialize_obligation + ATA +
    // refresh_farms + refresh_obligation + refresh_reserve + deposit).
    let mut ixs = deposit_ix(&user, &reserve, amount_lamports).context("build deposit_ix")?;

    // Track insertion offset: instructions prepended before initialize_obligation
    // shift the index of everything after them.
    let mut prefix_count: usize = 0;

    // For a fresh wallet, user_metadata must be initialized before
    // initialize_obligation can succeed. Prepend at position 0.
    if !user_metadata_exists(&ctx.rpc.client, &user).await {
        info!("user_metadata not found — prepending init_user_metadata_ix");
        ixs.insert(0, init_user_metadata_ix(&user));
        prefix_count += 1;
    }

    // If the reserve has a collateral farm, the obligationFarmUserState must be
    // initialized (once) before RefreshObligationFarmsForReserve can run.
    // It must go AFTER initialize_obligation (index prefix_count) so the
    // obligation account exists when the farms init touches it.
    if reserve.farm_collateral != Pubkey::default()
        && !obligation_farm_state_exists(&ctx.rpc.client, &reserve.farm_collateral, &user, &market)
            .await
    {
        info!("obligationFarmUserState not found — inserting init_obligation_farms_for_reserve_ix");
        // Insert after initialize_obligation_ix (which is at prefix_count).
        ixs.insert(
            prefix_count + 1,
            init_obligation_farms_for_reserve_ix(&user, &user, &reserve),
        );
    }

    Ok(ixs)
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

    let ixs = withdraw_ix(&user, &reserve, amount_lamports).context("build withdraw_ix")?;
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
}
