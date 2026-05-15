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
use std::sync::Arc;
use tracing::{info, warn};

use zerox1_defi_protocols::constants::{
    JLP_MINT, JLP_POOL, USDC_MINT, WBTC_PORTAL_MINT, WETH_PORTAL_MINT, WSOL_MINT,
};
use zerox1_defi_protocols::protocols::jlp::{
    add_liquidity_ix, decode_custody, derive_event_authority, derive_perpetuals,
    derive_transfer_authority, CustodyAccount, CustodyMeta, PoolMeta,
};
use zerox1_defi_protocols::protocols::pyth::decode_price;
use zerox1_defi_runtime::rpc::{classify_simulation, RpcContext};
use zerox1_protocol::fleet::hedgedjlp::{AssignHedgedJlp, ReportHedgedJlp};
use zerox1_protocol::fleet::ReportHeader;

use crate::delta::{compute_delta, CustodyExposure, PortfolioDelta};
use crate::dispatch::DispatchCtx;
use crate::hedge;

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

/// M8 entry point: compose the JLP-buy leg with the Jupiter Perps
/// hedge-open leg.
///
/// 1. Build + whitelist-verify + simulate/submit the JLP-buy ixns.
///    On JLP-buy failure, return an error Report and SKIP the hedge.
/// 2. Compute synthetic portfolio delta (`read_pool_state_or_synthetic`).
///    M9 wires the live custody-read path; for now we use a hard-coded
///    composition matching the JLP pool's published target weights.
/// 3. For each non-stable asset (SOL/ETH/BTC), build + whitelist-verify
///    + simulate/submit a `create_increase_position_request` ixn pair.
/// 4. Return a composed `ReportHedgedJlp` with all attempted tx_sigs
///    from both legs.
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
        "hedgedjlp run starting (M8 — JLP buy + hedge-leg request submit)"
    );

    // ── 1. JLP-buy leg ──────────────────────────────────────────────────
    let buy_sig_opt: Option<String> = match run_jlp_buy_only(ctx, payload, conv).await? {
        Ok(sig) => sig,
        Err(code) => {
            return Ok(error_report(conv, code));
        }
    };

    // ── 2. Compute portfolio delta ──────────────────────────────────────
    let delta = match read_pool_state_or_synthetic(payload).await {
        Ok(d) => d,
        Err(e) => {
            warn!(?conv, ?e, "delta-read failed; skipping hedge leg");
            // Buy succeeded; surface as success with hedge_notional=0.
            let mut sigs = Vec::new();
            if let Some(s) = buy_sig_opt {
                sigs.push(s);
            }
            return Ok(ReportHedgedJlp {
                header: ReportHeader::ok(conv),
                jlp_acquired_lamports: payload.usdc_lamports,
                hedge_notional_usdc: 0,
                current_delta_bps: 10_000, // unhedged
                tx_signatures: sigs,
            });
        }
    };

    // ── 3. Hedge-leg open requests ──────────────────────────────────────
    let hedge_result = match hedge::open_short_requests(ctx, payload, &delta).await {
        Ok(v) => v,
        Err(e) => {
            warn!(
                ?conv,
                ?e,
                "hedge::open_short_requests failed; reporting buy-only success"
            );
            crate::hedge::HedgeOpenResult {
                total_notional: 0,
                signatures: vec![],
                open_positions: vec![],
                sim_only: ctx.simulate_only,
            }
        }
    };

    let hedge_notional = hedge_result.total_notional;

    // ── 4. Compose Report ───────────────────────────────────────────────
    let mut all_sigs = Vec::new();
    if let Some(s) = buy_sig_opt {
        all_sigs.push(s);
    }
    for s in &hedge_result.signatures {
        all_sigs.push(s.to_string());
    }

    // Post-hedge delta as bps of total: current_long - hedge_notional,
    // then divided by total. Hedge can exceed current_long (net short
    // bias case), in which case the result is negative.
    let current_long = (delta.sol_usd as i128) + (delta.eth_usd as i128) + (delta.btc_usd as i128);
    let post_long_signed = current_long - (hedge_notional as i128);
    let total = delta.total_usd.max(1) as i128;
    let post_delta_bps =
        ((post_long_signed * 10_000) / total).clamp(i16::MIN as i128, i16::MAX as i128) as i16;

    // ── 5. Persist active position (audit-fix C1) ──────────────────────
    //
    // Only persist on a real submit. Sim-only runs do NOT call
    // `set_active_position` because no real on-chain position exists;
    // persisting would mislead the rebalancer + telemetry into thinking
    // there's a position to manage.
    if !hedge_result.sim_only && !hedge_result.open_positions.is_empty() {
        let pos = crate::rebalance::ActivePosition {
            conv,
            our_jlp_lamports: payload.usdc_lamports,
            jlp_acquired_lamports: payload.usdc_lamports,
            target_delta_bps: payload.target_delta_bps,
            max_borrow_rate_bps: payload.max_borrow_rate_bps,
            // v0: synthetic-custody path doesn't surface a custody
            // pubkey list. Once the live custody loader lands, this
            // should be populated from `read_pool_state`'s inputs so
            // the rebalancer + borrow-rate watch have something to
            // read. Empty list = rebalancer no-ops cleanly per
            // `tick_once`'s `custody_pubkeys.is_empty()` branch.
            custody_pubkeys: vec![],
            hedge_notional_usdc: hedge_notional,
            open_positions: hedge_result.open_positions.clone(),
        };
        ctx.state.set_active_position(pos);
        info!(
            ?conv,
            open_positions = hedge_result.open_positions.len(),
            "active position recorded into RebalanceState (audit-fix C1)"
        );
    } else if hedge_result.sim_only {
        info!(
            ?conv,
            "sim-only — not persisting active position (audit-fix C1)"
        );
    } else {
        info!(
            ?conv,
            "no successful hedge opens — not persisting active position"
        );
    }

    Ok(ReportHedgedJlp {
        header: ReportHeader::ok(conv),
        jlp_acquired_lamports: payload.usdc_lamports,
        hedge_notional_usdc: hedge_notional,
        current_delta_bps: post_delta_bps,
        tx_signatures: all_sigs,
    })
}

/// Run the JLP-buy leg only. Returns:
/// - `Ok(Ok(Some(sig)))` on submit success
/// - `Ok(Ok(None))` on simulate-only success (no signature)
/// - `Ok(Err(error_code))` if buy failed (caller surfaces as error Report)
/// - `Err(_)` only on whitelist-context fatal errors (rare)
async fn run_jlp_buy_only(
    ctx: &DispatchCtx,
    payload: &AssignHedgedJlp,
    conv: [u8; 16],
) -> Result<std::result::Result<Option<String>, u32>> {
    // Audit fix 9 (completion v0.2.2): prefer the live-loaded JLP pool's
    // USDC custody when available. The pool is loaded at boot in
    // dispatch.rs via `load_live_pool` and exposes the real
    // `token_account` + Pyth/Doves oracle pubkeys decoded from the
    // on-chain custody account body. Falling back to the synthetic
    // CustodyMeta only happens on devnet where the live read returns
    // None — in that path, `validate_custody_not_synthetic` still
    // hard-stops submit mode (audit-fix C3).
    let usdc_custody = match ctx
        .pool
        .as_ref()
        .and_then(|p| p.custody_for_mint(&USDC_MINT))
    {
        Some(c) => c.clone(),
        None => synthetic_jlp_usdc_custody(),
    };
    if let Err(e) = crate::hedge::validate_custody_not_synthetic(
        &usdc_custody,
        "JLP buy USDC custody",
        ctx.simulate_only,
    ) {
        warn!(?conv, ?e, "synthetic-custody hard-stop on JLP buy");
        return Ok(Err(ERROR_CODE_BUILD_FAILED));
    }

    let buy_ixs = match build_jlp_buy_ixns(ctx, payload, &usdc_custody).await {
        Ok(v) => v,
        Err(e) => {
            warn!(?conv, ?e, "JLP buy ixn build failed");
            return Ok(Err(ERROR_CODE_BUILD_FAILED));
        }
    };

    ctx.whitelist
        .verify_ixns(&buy_ixs)
        .context("whitelist check on JLP-buy ixns")?;
    info!(
        ?conv,
        ix_count = buy_ixs.len(),
        "JLP-buy whitelist check passed"
    );

    if ctx.simulate_only {
        info!(
            ?conv,
            "simulate_only=true — running build_sign_simulate on JLP buy"
        );
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
                        "JLP-buy simulation returned error \
                         (expected on devnet — Jupiter Perps mainnet-only)"
                    );
                    return Ok(Err(ERROR_CODE_SIM_FAILED));
                }
                info!(?conv, layout_valid, summary = %summary, "JLP-buy simulation succeeded");
                Ok(Ok(None))
            }
            Err(e) => {
                warn!(?conv, ?e, "JLP-buy build_sign_simulate threw");
                Ok(Err(ERROR_CODE_SIM_FAILED))
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
                Ok(Ok(Some(sig.to_string())))
            }
            Err(e) => {
                warn!(?conv, ?e, "JLP-buy build_sign_send failed");
                Ok(Err(ERROR_CODE_SIM_FAILED))
            }
        }
    }
}

/// Compute synthetic portfolio delta given `usdc_lamports` deposited.
///
/// M9 wires the live `decode_custody` reads + total JLP supply read.
/// For M8, we use the JLP pool's published target weights:
/// ~47% SOL, ~10% ETH, ~10% BTC, ~25% USDC, ~9% USDT.
///
/// Returns a `PortfolioDelta` shaped as if our deposit acquired pro-rata
/// shares of all five custodies — ie our synthetic JLP holdings have
/// the pool's average composition.
async fn read_pool_state_or_synthetic(payload: &AssignHedgedJlp) -> Result<PortfolioDelta> {
    // Synthetic composition: total = usdc_lamports (treat 1 USDC = 1 JLP
    // ≈ NAV proxy).  Build five CustodyExposure entries shaped to the
    // pool's published weights and feed them to `compute_delta` with
    // our_jlp = total = usdc_lamports for a 100%-share proportion.
    //
    // This is the same shape M9's live read will produce; only the
    // numbers differ.
    let total_micro_usd = payload.usdc_lamports;
    let custodies = vec![
        CustodyExposure {
            mint: WSOL_MINT,
            usd_value: scale_bps(total_micro_usd, 4_700), // 47%
            is_stable: false,
        },
        CustodyExposure {
            mint: WETH_PORTAL_MINT,
            usd_value: scale_bps(total_micro_usd, 1_000), // 10%
            is_stable: false,
        },
        CustodyExposure {
            mint: WBTC_PORTAL_MINT,
            usd_value: scale_bps(total_micro_usd, 1_000), // 10%
            is_stable: false,
        },
        CustodyExposure {
            mint: USDC_MINT,
            usd_value: scale_bps(total_micro_usd, 2_500), // 25%
            is_stable: true,
        },
        CustodyExposure {
            // USDT — bucketed as stable.
            mint: zerox1_defi_protocols::constants::USDT_MINT,
            usd_value: scale_bps(total_micro_usd, 900), // 9%
            is_stable: true,
        },
    ];
    // 100% pro-rata share: our JLP = total supply.
    compute_delta(&custodies, 1, 1).context("compute synthetic delta")
}

#[inline]
fn scale_bps(amount: u64, bps: u64) -> u64 {
    ((amount as u128) * (bps as u128) / 10_000) as u64
}

/// Live `read_pool_state` (M9): read JLP pool meta + each custody's
/// account body + each custody's Pyth oracle, compute per-custody
/// USD exposure, and return the resulting pro-rata `PortfolioDelta`.
///
/// Inputs:
/// - `rpc`: shared RPC context
/// - `our_jlp_lamports`: this daemon's JLP holdings (raw token units)
/// - `custody_pubkeys`: caller-supplied list of custody pubkeys (the
///   pool's `.custodies` list is encoded inside the pool account body
///   at variable offsets — a dedicated pool decoder is M11+ work, so
///   for v0 we accept the list as input and the rebalancer hard-codes
///   it from `defi-protocols::constants` once those land).
///
/// Returns `(PortfolioDelta, total_jlp_supply)` on success. On any RPC
/// or decode failure, returns the underlying error — caller (the
/// rebalancer or dispatch) decides whether to fall back to the
/// `read_pool_state_or_synthetic` shape.
///
/// On devnet this will fail (Jupiter Perps mainnet-only) — the daemon
/// must handle the error gracefully (log + skip rebalance tick).
pub async fn read_pool_state(
    rpc: &Arc<RpcContext>,
    our_jlp_lamports: u64,
    custody_pubkeys: &[Pubkey],
) -> Result<(PortfolioDelta, u64)> {
    // 1. JLP mint supply for the pro-rata share — read the mint account.
    //    Mint layout has supply at offset 36 (after 4 mint_authority option,
    //    32 authority pubkey).
    let mint_data = rpc
        .client
        .get_account_data(&JLP_MINT)
        .await
        .context("get_account_data for JLP_MINT (read_pool_state)")?;
    let total_jlp_supply = decode_mint_supply(&mint_data).context("decode JLP mint supply")?;

    // 2. For each custody pubkey: read account, decode_custody, read
    //    Pyth oracle, compute USD value.
    let mut exposures = Vec::with_capacity(custody_pubkeys.len());
    for cp in custody_pubkeys {
        let custody_data = rpc
            .client
            .get_account_data(cp)
            .await
            .with_context(|| format!("get_account_data for custody {cp}"))?;
        let custody: CustodyAccount = decode_custody(&custody_data)
            .map_err(|e| anyhow::anyhow!("decode_custody({}): {:?}", cp, e))?;

        // Compute per-custody USD value.
        let usd_value = if custody.is_stable {
            // Stable custody: 1:1 USD value (in micro-USD, normalized by decimals).
            scale_owned_to_micro_usd_stable(custody.assets.owned, custody.decimals)
        } else {
            // Non-stable: read Pyth and multiply.
            let pyth_data = rpc
                .client
                .get_account_data(&custody.pythnet_price_account)
                .await
                .with_context(|| {
                    format!(
                        "get_account_data for pyth oracle {}",
                        custody.pythnet_price_account
                    )
                })?;
            let pyth =
                decode_price(&pyth_data).map_err(|e| anyhow::anyhow!("decode_price: {:?}", e))?;
            scale_owned_to_micro_usd(
                custody.assets.owned,
                custody.decimals,
                pyth.price,
                pyth.expo,
            )
        };

        exposures.push(CustodyExposure {
            mint: custody.mint,
            usd_value,
            is_stable: custody.is_stable,
        });
    }

    // 3. Compose the delta.
    let delta = compute_delta(&exposures, our_jlp_lamports, total_jlp_supply)
        .context("compute_delta from live custodies")?;
    Ok((delta, total_jlp_supply))
}

// ── Live custody loader (audit fix 9) ──────────────────────────────────────
//
// Mainnet JLP pool custody pubkeys per spec §6. Stable since program
// launch. Audit-fix 9: replace the synthetic-everywhere CustodyMeta
// with `decode_custody` reads against these on-chain accounts.
pub const JLP_SOL_CUSTODY: Pubkey =
    solana_sdk::pubkey!("7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz");
pub const JLP_BTC_CUSTODY: Pubkey =
    solana_sdk::pubkey!("5Pv3gM9JrFFH883SWAhvJC9RPYmo8UNxuFtv5bMMALkm");
pub const JLP_ETH_CUSTODY: Pubkey =
    solana_sdk::pubkey!("AQCGyheWPLeo6Qp9WpYS9m3Qj479t7R636N9ey1rEjEn");
pub const JLP_USDC_CUSTODY_ADDR: Pubkey =
    solana_sdk::pubkey!("G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa");
pub const JLP_USDT_CUSTODY: Pubkey =
    solana_sdk::pubkey!("4vkNeXiYEUizLdrpdPS1eC2mccyM4NUPRtERrk6ZETkk");

/// Mainnet custody pubkeys in pool order (spec §6).
pub const MAINNET_CUSTODY_PUBKEYS: &[Pubkey] = &[
    JLP_SOL_CUSTODY,
    JLP_BTC_CUSTODY,
    JLP_ETH_CUSTODY,
    JLP_USDC_CUSTODY_ADDR,
    JLP_USDT_CUSTODY,
];

/// Audit fix 9: load the live JLP `PoolMeta` from on-chain custody
/// reads, replacing the M6/M8 synthetic CustodyMeta stand-ins. Returns
/// `None` on devnet (Jupiter Perps is mainnet-only) — caller falls
/// back to synthetic-with-hard-stop.
pub async fn load_live_pool(rpc: &Arc<RpcContext>) -> Result<PoolMeta> {
    let custody_pubkeys = MAINNET_CUSTODY_PUBKEYS.to_vec();
    let accounts = rpc
        .client
        .get_multiple_accounts(&custody_pubkeys)
        .await
        .context("get_multiple_accounts for JLP custodies")?;

    let mut custodies = Vec::with_capacity(custody_pubkeys.len());
    for (pk, maybe_acct) in custody_pubkeys.iter().zip(accounts.into_iter()) {
        let acct =
            maybe_acct.with_context(|| format!("custody {pk} not present on this cluster"))?;
        let decoded = decode_custody(&acct.data)
            .map_err(|e| anyhow::anyhow!("decode_custody({pk}): {:?}", e))?;
        custodies.push(decoded.to_custody_meta(*pk));
    }

    Ok(PoolMeta {
        pool: JLP_POOL,
        jlp_mint: JLP_MINT,
        perpetuals: derive_perpetuals(),
        transfer_authority: derive_transfer_authority(),
        event_authority: derive_event_authority(),
        custodies,
    })
}

/// SPL Mint account layout: supply is at byte offset 36.
fn decode_mint_supply(data: &[u8]) -> Result<u64> {
    if data.len() < 44 {
        anyhow::bail!("mint account too short ({} < 44)", data.len());
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[36..44]);
    Ok(u64::from_le_bytes(buf))
}

/// Convert a stable-coin `owned` (raw mint units at `decimals` decimals)
/// into micro-USD ($1 = 1_000_000). Stable values are 1:1, so we just
/// rescale the decimal place.
pub(crate) fn scale_owned_to_micro_usd_stable(owned: u64, decimals: u8) -> u64 {
    let target_decimals: i32 = 6; // micro-USD = 6 decimals
    let actual: i32 = decimals as i32;
    let owned_u128 = owned as u128;
    let result = if actual >= target_decimals {
        let div_power = (actual - target_decimals) as u32;
        owned_u128 / 10u128.pow(div_power)
    } else {
        let mul_power = (target_decimals - actual) as u32;
        owned_u128.saturating_mul(10u128.pow(mul_power))
    };
    result.min(u64::MAX as u128) as u64
}

/// Convert raw `owned` mint units + Pyth (price, expo) into micro-USD.
///
/// Pyth: real_price = price * 10^expo (expo is typically negative).
/// owned (mint scale) → real units = owned / 10^decimals.
/// usd = real_units * real_price = owned * price * 10^(expo - decimals).
/// To get micro-USD ($1=1e6): multiply by 1e6:
///   usd_micro = owned * price * 10^(expo - decimals + 6).
///
/// We compute in i128 then clip to u64. Negative prices clip to 0.
pub(crate) fn scale_owned_to_micro_usd(owned: u64, decimals: u8, price: i64, expo: i32) -> u64 {
    if price <= 0 {
        return 0;
    }
    let exponent: i32 = expo - (decimals as i32) + 6;
    let mantissa: i128 = (owned as i128).saturating_mul(price as i128);
    let result_i128 = if exponent >= 0 {
        mantissa.saturating_mul(10i128.pow(exponent as u32))
    } else {
        let div = 10i128.pow((-exponent) as u32);
        mantissa / div
    };
    if result_i128 < 0 {
        return 0;
    }
    result_i128.min(u64::MAX as i128) as u64
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
    usdc_custody: &CustodyMeta,
) -> Result<Vec<Instruction>> {
    if payload.usdc_lamports == 0 {
        anyhow::bail!("usdc_lamports must be > 0");
    }

    let user = ctx.wallet.pubkey();

    // Audit fix 9 (completion v0.2.2): prefer the live-loaded pool so
    // the buy ixn carries the real token_account + oracle pubkeys for
    // the USDC custody (and all sibling custodies — Jupiter Perps reads
    // all of them for AUM math). Fall back to the synthetic single-
    // custody PoolMeta only when no live pool was loaded (devnet).
    let pool = match &ctx.pool {
        Some(p) => (**p).clone(),
        None => PoolMeta {
            pool: JLP_POOL,
            jlp_mint: JLP_MINT,
            perpetuals: derive_perpetuals(),
            transfer_authority: derive_transfer_authority(),
            event_authority: derive_event_authority(),
            custodies: vec![usdc_custody.clone()],
        },
    };

    // M6 disables slippage protection (min_lp_amount_out = 0). M7+
    // computes the expected output via `getAddLiquidityAmountAndFee2`
    // and applies a real slippage bound. Safe for sim-only and for
    // mainnet runs gated behind the approval queue (operator inspects
    // the simulated amount before approving).
    let ixs = add_liquidity_ix(&user, &pool, usdc_custody, payload.usdc_lamports, 0)
        .context("build add_liquidity_ix")?;

    Ok(ixs)
}

/// Construct the synthetic CustodyMeta used by the JLP-buy leg. v0
/// uses placeholder pubkeys for `token_account` + the two oracle
/// fields — these get replaced once the live custody loader lands.
/// Audit-fix C3 detects this synthetic shape and refuses to submit
/// in mainnet mode (sim-only logs warn).
fn synthetic_jlp_usdc_custody() -> CustodyMeta {
    CustodyMeta {
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
    }
}

/// Build a successful `ReportHedgedJlp` in the M6 shape (100% long, no
/// hedge). Kept as a test helper for the M6 single-leg invariants —
/// M8's `run_or_simulate` builds the composed Report inline so it can
/// reflect the actual hedge_notional / post-hedge delta. Removed in
/// M9 once the test surface fully migrates to live-pool reads.
#[allow(dead_code)]
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
        assert_eq!(
            r.current_delta_bps, 10_000,
            "M6 must report 100% long until M8 lands hedge"
        );
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

    // ── M9: read_pool_state helpers ────────────────────────────────────

    #[test]
    fn decode_mint_supply_reads_offset_36() {
        // SPL Mint layout:
        //   [0..36]  mint_authority option + pubkey
        //   [36..44] supply (u64 LE)
        let mut data = vec![0u8; 82]; // SPL mint = 82 bytes
        let supply: u64 = 1_234_567_890_000;
        data[36..44].copy_from_slice(&supply.to_le_bytes());
        let got = decode_mint_supply(&data).expect("decode");
        assert_eq!(got, supply);
    }

    #[test]
    fn decode_mint_supply_rejects_short_slice() {
        let data = vec![0u8; 40];
        assert!(decode_mint_supply(&data).is_err());
    }

    #[test]
    fn scale_owned_to_micro_usd_stable_usdc_6_decimals() {
        // USDC has 6 decimals; owned=1_000_000 (raw) = $1 = 1_000_000 micro-USD.
        assert_eq!(scale_owned_to_micro_usd_stable(1_000_000, 6), 1_000_000);
    }

    #[test]
    fn scale_owned_to_micro_usd_stable_9_decimals_scales_down() {
        // 9-decimal stable: owned=1_000_000_000 (raw) = $1 = 1_000_000 micro-USD.
        assert_eq!(scale_owned_to_micro_usd_stable(1_000_000_000, 9), 1_000_000);
    }

    #[test]
    fn scale_owned_to_micro_usd_sol_at_100_dollars() {
        // 1 SOL (decimals=9) at price=100 (expo=0) = $100 = 100_000_000 micro-USD.
        // Pyth wouldn't actually use expo=0 for SOL but math should hold.
        let owned: u64 = 1_000_000_000; // 1 SOL raw
        let usd = scale_owned_to_micro_usd(owned, 9, 100, 0);
        // exponent = 0 - 9 + 6 = -3; mantissa = 1e9 * 100 = 1e11; / 1e3 = 1e8.
        assert_eq!(usd, 100_000_000);
    }

    #[test]
    fn scale_owned_to_micro_usd_pyth_realistic_expo_neg_8() {
        // 1 SOL @ price = 10_000_000_000, expo = -8 → real price = $100.
        // Same expected result: $100 = 100_000_000 micro-USD.
        let owned: u64 = 1_000_000_000;
        let usd = scale_owned_to_micro_usd(owned, 9, 10_000_000_000, -8);
        // exponent = -8 - 9 + 6 = -11; mantissa = 1e9 * 1e10 = 1e19; / 1e11 = 1e8.
        assert_eq!(usd, 100_000_000);
    }

    #[test]
    fn scale_owned_to_micro_usd_negative_price_clips_to_zero() {
        let usd = scale_owned_to_micro_usd(1_000_000_000, 9, -1, -8);
        assert_eq!(usd, 0);
    }

    #[test]
    fn scale_owned_to_micro_usd_zero_owned() {
        let usd = scale_owned_to_micro_usd(0, 9, 100, 0);
        assert_eq!(usd, 0);
    }

    // ── Audit-fix C1: set_active_position decision logic ───────────────

    #[test]
    fn synthetic_jlp_usdc_custody_is_flagged_synthetic() {
        // The buy-leg helper produces a CustodyMeta with all three
        // address fields == JLP_USDC_CUSTODY. validate_custody_*()
        // must catch this in submit mode so the C3 hard-stop fires.
        let c = synthetic_jlp_usdc_custody();
        assert_eq!(c.token_account, c.address);
        assert_eq!(c.pythnet_price_account, c.address);
        assert_eq!(c.doves_price_account, c.address);
        let r = crate::hedge::validate_custody_not_synthetic(
            &c, "test-buy", /*simulate_only*/ false,
        );
        assert!(
            r.is_err(),
            "synthetic JLP-USDC custody must hard-stop submit"
        );
    }

    #[test]
    fn synthetic_jlp_usdc_custody_passes_in_sim_mode() {
        let c = synthetic_jlp_usdc_custody();
        let r = crate::hedge::validate_custody_not_synthetic(
            &c, "test-buy", /*simulate_only*/ true,
        );
        assert!(r.is_ok(), "synthetic custody must warn-only in sim mode");
    }

    #[test]
    fn active_position_persists_with_3tuple_open_positions() {
        // Audit-fix C1 + C2 round-trip via RebalanceState. The Active
        // position carrying real `(label, pubkey, counter)` triples
        // must round-trip through set + snapshot without loss.
        use crate::rebalance::{ActivePosition, RebalanceState};
        use solana_sdk::pubkey::Pubkey;
        let state = RebalanceState::new();
        let p1 = Pubkey::new_unique();
        let p2 = Pubkey::new_unique();
        let pos = ActivePosition {
            conv: [7u8; 16],
            our_jlp_lamports: 100,
            jlp_acquired_lamports: 100,
            target_delta_bps: 0,
            max_borrow_rate_bps: 5_000,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 130_000_000,
            open_positions: vec![
                ("SOL".to_string(), p1, 1_700_000_001),
                ("ETH".to_string(), p2, 1_700_000_002),
            ],
        };
        state.set_active_position(pos.clone());
        let snap = state.snapshot_active_position().expect("active");
        assert_eq!(snap.open_positions.len(), 2);
        assert_eq!(snap.open_positions[0].0, "SOL");
        assert_eq!(snap.open_positions[0].1, p1);
        assert_eq!(snap.open_positions[0].2, 1_700_000_001);
        assert_eq!(snap.open_positions[1].2, 1_700_000_002);
    }
}
