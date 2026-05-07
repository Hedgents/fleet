//! JLP-buy leg via Jupiter Perps `add_liquidity_2`.
//!
//! M6 lands the buy half of the hedged-JLP strategy: deposit USDC into
//! the Jupiter Perps pool and mint JLP at NAV (no aggregator routing —
//! direct to the pool). The hedge-leg open (Jupiter Perps short via
//! 2-tx request flow) lands in M8; this file currently does only the
//! buy and returns a Report with `current_delta_bps = 10_000` (100%
//! long, no hedge yet) and `hedge_notional_usdc = 0`.
//!
//! Compute budget ixns (set_compute_unit_limit + set_compute_unit_price)
//! are NOT pushed here — `RpcContext::build_signed` prepends them
//! automatically and the whitelist already covers compute_budget::ID
//! (mirrors stable-yield M6's deviation note).
//!
//! Audit-fix I1: `SigningWhitelist::verify_ixns` runs before signing on
//! BOTH the sim-only and submit paths. Any ixn whose `program_id` falls
//! outside `whitelist::whitelist_program_ids` is rejected before the
//! wallet ever sees the message.

use anyhow::{Context, Result};
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn};

use zerox1_defi_protocols::constants::{JLP_MINT, JLP_POOL, USDC_MINT};
use zerox1_defi_protocols::protocols::jlp::{
    add_liquidity_ix, derive_event_authority, derive_perpetuals, derive_transfer_authority,
    CustodyMeta, PoolMeta,
};
use zerox1_defi_runtime::rpc::classify_simulation;
use zerox1_protocol::fleet::hedgedjlp::{AssignHedgedJlp, ReportHedgedJlp};
use zerox1_protocol::fleet::ReportHeader;

use crate::dispatch::DispatchCtx;

/// Jupiter Perps `add_liquidity_2` plus two idempotent ATA-creates fits
/// well under 400k. Bumped to 600k vs stable-yield's 400k because the
/// perps program does more pool math (AUM read + price oracle reads
/// for two pyth feeds + custody updates).
const JLP_BUY_CU_LIMIT: u32 = 600_000;
/// Same priority fee envelope as stable-yield. Mainnet promotion may
/// tune this upward in M12.
const JLP_BUY_PRIORITY_FEE: u64 = 10_000;

/// Error code emitted when build_sign_simulate / build_sign_send returns
/// a TransactionError. Matches stable-yield M6's coding convention so
/// operators can grep across both daemons consistently.
const ERROR_CODE_SIM_FAILED: u32 = 5;
/// Error code emitted when the JLP-buy ixn-build path blows up before
/// we even reach simulate/submit (e.g. zero amount, custody-derivation
/// crash). Distinct from sim-failed so it's grep-able.
const ERROR_CODE_BUILD_FAILED: u32 = 6;

/// Mainnet JLP-USDC custody (verified against on-chain pool state on
/// 2026-05-04). The custody is a PDA owned by Jupiter Perps holding
/// the deposited USDC; its address is part of the pool layout and
/// stable across program upgrades. Used as the fallback when the
/// on-chain custody read isn't wired (M6 keeps a synthetic path so
/// the daemon stays meaningful on devnet).
///
/// The accompanying token vault, oracle accounts, etc. are NOT
/// constants — they live inside the custody account body and would
/// need a 2000-byte read + offset decode. M6 uses synthetic stand-ins
/// for those fields; M7+ can wire a real loader (the protocol crate
/// already documents the offsets in `jlp.rs` lines 40-51).
const JLP_USDC_CUSTODY: Pubkey =
    solana_sdk::pubkey!("G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa");

/// Build the JLP-buy ixns, run them through `SigningWhitelist::verify_ixns`,
/// then either simulate (sim-only) or broadcast.
///
/// The hedge-leg open (Jupiter Perps short via 2-tx request flow) lands
/// in M8 — for now we always return `current_delta_bps = 10_000`
/// (100% long, no hedge) and `hedge_notional_usdc = 0`.
pub async fn run_or_simulate(
    ctx: &DispatchCtx,
    payload: &AssignHedgedJlp,
    conv: [u8; 16],
) -> Result<ReportHedgedJlp> {
    info!(
        ?conv,
        usdc_lamports = payload.usdc_lamports,
        target_delta_bps = payload.target_delta_bps,
        max_borrow_rate_bps = payload.max_borrow_rate_bps,
        simulate_only = ctx.simulate_only,
        "JLP buy starting (M6 — buy leg only; hedge open lands in M8)"
    );

    // Build phase. Catch any anyhow error here and convert it to a
    // build-failed Report so a derivation crash or zero-amount payload
    // doesn't kill the daemon.
    let buy_ixs = match build_jlp_buy_ixns(ctx, payload).await {
        Ok(v) => v,
        Err(e) => {
            warn!(?conv, ?e, "JLP buy ixn build failed");
            return Ok(error_report(conv, ERROR_CODE_BUILD_FAILED));
        }
    };

    // Audit-fix I1: structural authority boundary. Every ixn in the
    // bundle must target a program in the daemon's signing whitelist.
    // RpcContext additionally prepends two compute-budget ixns, which
    // are also covered by the whitelist (compute_budget::ID).
    ctx.whitelist
        .verify_ixns(&buy_ixs)
        .context("whitelist check on JLP-buy ixns")?;
    info!(?conv, ix_count = buy_ixs.len(), "whitelist check passed");

    if ctx.simulate_only {
        info!(?conv, "simulate_only=true — running build_sign_simulate");
        match ctx
            .rpc
            .build_sign_simulate(
                buy_ixs,
                ctx.wallet.keypair(),
                JLP_BUY_CU_LIMIT,
                JLP_BUY_PRIORITY_FEE,
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
                        "simulation returned error \
                         (expected on devnet — Jupiter Perps is mainnet-only)"
                    );
                    return Ok(error_report(conv, ERROR_CODE_SIM_FAILED));
                }
                info!(?conv, layout_valid, summary = %summary, "simulation succeeded");
                Ok(success_report(conv, payload.usdc_lamports, None))
            }
            Err(e) => {
                warn!(?conv, ?e, "build_sign_simulate threw");
                Ok(error_report(conv, ERROR_CODE_SIM_FAILED))
            }
        }
    } else {
        info!(?conv, "submit path — broadcasting JLP buy");
        match ctx
            .rpc
            .build_sign_send(
                buy_ixs,
                ctx.wallet.keypair(),
                JLP_BUY_CU_LIMIT,
                JLP_BUY_PRIORITY_FEE,
            )
            .await
        {
            Ok(sig) => {
                info!(?conv, %sig, "JLP buy confirmed on-chain");
                Ok(success_report(
                    conv,
                    payload.usdc_lamports,
                    Some(sig.to_string()),
                ))
            }
            Err(e) => {
                warn!(?conv, ?e, "build_sign_send failed");
                Ok(error_report(conv, ERROR_CODE_SIM_FAILED))
            }
        }
    }
}

/// Build the JLP-buy ixn bundle: idempotent ATA-create for input USDC,
/// idempotent ATA-create for JLP output, and `add_liquidity_2`.
///
/// Three ixns total — the ATA-creates are emitted by `add_liquidity_ix`
/// itself so we don't double-create. See `jlp.rs` lines 156-218.
///
/// M6 uses a synthetic `CustodyMeta` for the USDC custody — the real
/// addresses for `token_account`, `pythnet_price_account`,
/// `doves_price_account` live inside the on-chain custody account body
/// (~2000 bytes, fixed offsets per `jlp.rs` lines 40-51). A live loader
/// is M7+ work. For M6 the wiring + whitelist are the lift; the live
/// simulation is expected to fail on devnet (program not deployed) and
/// on mainnet pre-loader (synthetic oracle pubkeys won't pass account
/// validation).
async fn build_jlp_buy_ixns(
    ctx: &DispatchCtx,
    payload: &AssignHedgedJlp,
) -> Result<Vec<Instruction>> {
    if payload.usdc_lamports == 0 {
        anyhow::bail!("usdc_lamports must be > 0");
    }

    let user = ctx.wallet.pubkey();

    // Synthetic CustodyMeta — real values land in M7+ via a live loader.
    // Using `JLP_USDC_CUSTODY` for `address` (the only known mainnet
    // custody pubkey) and stand-ins for the rest. Sim will reject
    // account-data, which is the intended shape of the M6 smoke.
    let usdc_custody = CustodyMeta {
        address: JLP_USDC_CUSTODY,
        mint: USDC_MINT,
        // Token vault, pyth oracle, doves oracle: real addresses live
        // inside the custody account body. Use the custody address itself
        // as a stand-in so verify_ixns still runs and sim still surfaces
        // a real account-validation error. M7+ replaces these with the
        // decoded values.
        token_account: JLP_USDC_CUSTODY,
        pythnet_price_account: JLP_USDC_CUSTODY,
        doves_price_account: JLP_USDC_CUSTODY,
        decimals: 6,
        is_stable: true,
    };

    let pool = PoolMeta {
        pool: JLP_POOL,
        jlp_mint: JLP_MINT,
        perpetuals: derive_perpetuals(),
        transfer_authority: derive_transfer_authority(),
        event_authority: derive_event_authority(),
        custodies: vec![usdc_custody.clone()],
    };

    // M6 disables slippage protection (min_lp_amount_out = 0). M7+
    // computes the expected output via `getAddLiquidityAmountAndFee2`
    // and applies a real slippage bound. Safe for sim-only and for
    // mainnet runs gated behind the approval queue (operator inspects
    // the simulated amount before approving).
    let ixs = add_liquidity_ix(&user, &pool, &usdc_custody, payload.usdc_lamports, 0)
        .context("build add_liquidity_ix")?;

    Ok(ixs)
}

/// Build a successful `ReportHedgedJlp`. M6: `current_delta_bps = 10_000`
/// (100% long, no hedge yet) and `hedge_notional_usdc = 0`. M8 wires
/// the real hedge values.
fn success_report(
    conv: [u8; 16],
    usdc_lamports: u64,
    tx_signature: Option<String>,
) -> ReportHedgedJlp {
    ReportHedgedJlp {
        header: ReportHeader::ok(conv),
        // M6 proxy: requested USDC = expected JLP (NAV is ~$1 per
        // JLP token in M6's USDC-only deposit; real post-trade balance
        // read is M7+).
        jlp_acquired_lamports: usdc_lamports,
        // No hedge yet — M8 lands the short open.
        hedge_notional_usdc: 0,
        // 100% long until the hedge lands.
        current_delta_bps: 10_000,
        tx_signatures: tx_signature.map(|s| vec![s]).unwrap_or_default(),
    }
}

/// Build an error `ReportHedgedJlp` with the given error code. All
/// numeric fields zero — the orchestrator reads `header.ok` first.
fn error_report(conv: [u8; 16], code: u32) -> ReportHedgedJlp {
    ReportHedgedJlp {
        header: ReportHeader::err(conv, code),
        jlp_acquired_lamports: 0,
        hedge_notional_usdc: 0,
        current_delta_bps: 0,
        tx_signatures: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cu_limit_sane() {
        // add_liquidity_2 + 2 ATA creates fits comfortably under 600k.
        assert!(JLP_BUY_CU_LIMIT >= 200_000);
        assert!(JLP_BUY_CU_LIMIT <= 800_000);
    }

    #[test]
    fn error_codes_distinct() {
        assert_ne!(ERROR_CODE_SIM_FAILED, ERROR_CODE_BUILD_FAILED);
    }

    #[test]
    fn success_report_shape_m6_invariants() {
        // M6: 100% long (no hedge yet), zero hedge notional, requested
        // USDC echoed as JLP-acquired proxy.
        let conv = [0u8; 16];
        let r = success_report(conv, 200_000_000, None);
        assert!(r.header.ok);
        assert_eq!(r.jlp_acquired_lamports, 200_000_000);
        assert_eq!(r.hedge_notional_usdc, 0);
        assert_eq!(r.current_delta_bps, 10_000, "M6 must report 100% long until M8 lands hedge");
        assert!(r.tx_signatures.is_empty());
    }

    #[test]
    fn success_report_with_tx_sig_includes_one_entry() {
        let conv = [0u8; 16];
        let r = success_report(conv, 100_000_000, Some("sig-abc".to_string()));
        assert_eq!(r.tx_signatures, vec!["sig-abc".to_string()]);
    }

    #[test]
    fn error_report_zeroes_all_numeric_fields() {
        let conv = [0u8; 16];
        let r = error_report(conv, ERROR_CODE_SIM_FAILED);
        assert!(!r.header.ok);
        assert_eq!(r.header.error_code, Some(ERROR_CODE_SIM_FAILED));
        assert_eq!(r.jlp_acquired_lamports, 0);
        assert_eq!(r.hedge_notional_usdc, 0);
        assert_eq!(r.current_delta_bps, 0);
        assert!(r.tx_signatures.is_empty());
    }

    #[test]
    fn jlp_usdc_custody_is_set() {
        // Smoke: the constant must not be all-zeros (would wedge sim).
        assert_ne!(JLP_USDC_CUSTODY, Pubkey::default());
    }
}
