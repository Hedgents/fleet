//! Rebalance-resize action.
//!
//! Closes the M9 gap: when the periodic rebalancer detects drift past
//! `MAX_DELTA_DRIFT_BPS`, this module computes which hedge legs are
//! *missing* (per-asset target minus per-asset existing short), enqueues
//! that plan for operator approval through the same `ApprovalQueue`
//! generic Assign / Withdraw use, and on approve opens just the missing
//! shorts — never re-opening legs that already exist on-chain.
//!
//! ## Design (and what it deliberately does NOT do)
//!
//! - **No JLP buy.** Resize only adds shorts; it never re-deposits USDC
//!   into the JLP pool. The buy half is the Assign protocol's job.
//! - **No re-opens.** Per asset, `to_open = max(0, target − current)`.
//!   Assets where current ≥ target are skipped with a `Skip::AlreadyHedged`
//!   entry in the `ResizeOutcome` so the rebalancer can log the decision.
//! - **No new envelope type.** The trigger fires from inside the
//!   rebalancer tick; the operator approves via the existing Approve
//!   envelope. Dispatch routes the Approve to a resize-specific queue
//!   (the third instance of the generic approval queue), keeping audit-
//!   fix C1's sender-match check intact.
//! - **Same caps as Assign.** `MAX_POSITION_USDC_LAMPORTS` and
//!   `MAX_BORROW_RATE_BPS_HARDCAP` apply. A target exceeding the cap is
//!   clamped down and surfaces a `cap_hit_usdc` field in the outcome —
//!   we never silently violate the cap.
//! - **Idempotent on retry.** `compute_legs_to_open` reads the live
//!   `ActivePosition.open_positions` + `hedge_notional_usdc`. If a
//!   previous resize already executed, the second call returns empty
//!   work and `run_resize` returns `Queued::Nothing`.
//!
//! ## fleet-v0.4.1: recovered positions are now withdraw-capable
//!
//! Previously this module noted an "honest limitation": `recover.rs`
//! couldn't reconstruct the original `open_counter` for positions
//! observed on-chain at boot, so withdraw mis-derived the close-request
//! PDA. The fix landed in `unwind.rs`: the close path now reads the
//! on-chain `Position` account and generates a fresh close-counter
//! (per spec §3.6 the counter is just a randomization nonce, not a
//! structural link between open and close). The `(label, pubkey)`
//! tuple is the only state the unwind path needs.
//!
//! Resize still uses a real counter at *open* time inside its own
//! `derive_position_request(_, _, Increase)` call (same formula as
//! `hedge::open_short_requests`) — that's a local, per-call value and
//! does not need to be persisted onto `ActivePosition.open_positions`.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use tracing::{info, warn};

use zerox1_defi_protocols::constants::{USDC_MINT, WBTC_PORTAL_MINT, WETH_PORTAL_MINT, WSOL_MINT};
use zerox1_defi_protocols::protocols::jlp::{
    create_increase_position_request_ix, derive_position, derive_position_request, PerpSide,
    PoolMeta, RequestChange,
};
use zerox1_defi_runtime::identity::RoleIdentity;
use zerox1_defi_runtime::rpc::{classify_simulation, RpcContext};
use zerox1_defi_wallet::{SigningWhitelist, Wallet};
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::envelope::Envelope;
use zerox1_protocol::fleet::riskwatcher::{EscalateRisk, RiskKind, RiskSeverity};
use zerox1_protocol::message::MsgType;

use crate::caps::{MAX_BORROW_RATE_BPS_HARDCAP, MAX_POSITION_USDC_LAMPORTS};
use crate::delta::PortfolioDelta;
use crate::hedge::{validate_custody_not_synthetic, MIN_HEDGE_NOTIONAL_USD};
use crate::rebalance::{ActivePosition, RebalanceState};

/// Per-asset compute-unit ceiling for a single resize open-request ixn
/// pair. Same envelope as `hedge.rs` — the request shape is identical.
const RESIZE_CU_LIMIT: u32 = 600_000;
const RESIZE_PRIORITY_FEE: u64 = 10_000;
const RESIZE_FILL_VERIFY_ATTEMPTS: u32 = 20;
const RESIZE_FILL_VERIFY_DELAY: Duration = Duration::from_secs(1);

/// Hedge-leg leverage. Pinned to the same 5x the Assign open path uses
/// (`hedge.rs::HEDGE_LEVERAGE`). Kept private here so a future tweak
/// only needs to touch one constant in one module.
const RESIZE_LEVERAGE: u64 = 5;

/// What the rebalancer wants the operator to approve. Carries a per-
/// asset list of `(label, notional_usdc_micro)` legs to open. The
/// approval queue stores this verbatim and the dispatch layer hands it
/// to `execute_resize` on Approve.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResizePlan {
    /// `(asset_label, notional_usd_micro)`. Asset labels are always
    /// "SOL" / "ETH" / "BTC" (mirrors `hedge::AssetSlice`).
    pub legs: Vec<(String, u64)>,
    /// Snapshot of the delta read that drove the resize. Recorded for
    /// telemetry / audit — execute_resize re-reads chain state before
    /// each per-leg open so a stale snapshot does not influence sizing.
    pub observed_drift_bps: i32,
    /// `target_delta_bps` carried over from the active position at the
    /// time the resize was computed. Pinned so re-validation in
    /// execute_resize can re-check caps against the same target the
    /// operator approved.
    pub target_delta_bps: i16,
}

/// One reason a leg might be skipped from the resize. Kept as a typed
/// enum (not a string) so callers can match + log structurally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Target ≤ current short — no work needed for this asset.
    AlreadyHedged,
    /// Per-asset delta `< MIN_HEDGE_NOTIONAL_USD` (dust).
    BelowMinNotional,
    /// Asset has zero long exposure on this pool — nothing to hedge.
    ZeroExposure,
    /// Wallet's free USDC (in its ATA) can't fund this leg's collateral
    /// requirement even after proportional scale-down across all legs.
    /// fleet-v0.4.0-rc7: pre-flight gate added after the first on-chain
    /// rebalance attempt revealed a Jupiter Perps `Transfer
    /// InsufficientFunds (0x1)` error when the daemon optimistically
    /// signed an ETH-leg open against an under-funded USDC ATA. The
    /// daemon now reads free USDC up-front and surfaces this typed
    /// reason instead of leaving a 1200-line `custom program error: 0x1`
    /// log for the operator to decode. See `prices::fetch_wallet_free_usdc_lamports`.
    InsufficientUsdcLiquidity,
}

/// What the rebalancer learns from one resize attempt. Carries both
/// queued + skipped entries so the tick log surfaces every decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResizeOutcome {
    /// Legs queued for approval, in the order they will be opened.
    pub queued: Vec<(String, u64)>,
    /// Legs skipped this tick with the reason — for telemetry / logs.
    pub skipped: Vec<(String, SkipReason)>,
    /// If a leg was clamped down by `MAX_POSITION_USDC_LAMPORTS`, this
    /// records the amount of USDC the cap shaved off (summed across
    /// legs). Zero on the common path. `None` when no cap fired.
    pub cap_hit_usdc: Option<u64>,
    /// `true` iff the plan was non-empty AND we successfully enqueued.
    /// A queue-full or empty-plan outcome leaves this `false`.
    pub queued_to_approval: bool,
}

impl ResizeOutcome {
    fn empty() -> Self {
        Self {
            queued: vec![],
            skipped: vec![],
            cap_hit_usdc: None,
            queued_to_approval: false,
        }
    }
}

/// Bundle of dependencies the rebalancer hands to `run_resize`. Mirrors
/// the slice of `DispatchCtx` the resize path actually touches — kept
/// separate so the rebalancer tick can pass its own state (rpc, handle,
/// role, nonce, state) without taking a hard dep on the full dispatch
/// context.
pub struct ResizeCtx {
    pub rpc: Arc<RpcContext>,
    pub handle: NodeHandle,
    pub role: RoleIdentity,
    pub nonce: Arc<std::sync::atomic::AtomicU64>,
    pub state: Arc<RebalanceState>,
    pub wallet: Arc<Wallet>,
    pub whitelist: Arc<SigningWhitelist>,
    pub pool: Option<Arc<PoolMeta>>,
    pub simulate_only: bool,
    /// When `false`, resize plans execute immediately (no orchestrator
    /// Approve needed). Mirrors `DispatchCtx.require_approval` — same
    /// flag the Assign / Withdraw paths use. Set to `false` on devnet
    /// or when the operator explicitly passes `--require-approval=false`.
    pub require_approval: bool,
    pub resize_queue: Arc<crate::approval::ResizeApprovalQueue>,
    /// 32-byte pubkey of the orchestrator authorised to Approve a
    /// queued resize. Mirrors `DispatchCtx.orchestrator_agent_id` —
    /// `None` on devnet sandbox (any sender can approve). Used by
    /// `enqueue` so the existing `ApprovalQueue::approve` sender-match
    /// path (audit-fix C1) gates the resize approval too.
    pub orchestrator_agent_id: Option<[u8; 32]>,
}

/// Per-asset entry the pure compute function emits. Public so the
/// rebalancer (and tests) can introspect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PerAssetTarget {
    pub label: &'static str,
    pub mint: Pubkey,
    /// Target hedge notional for this asset in micro-USD ($1 = 1e6).
    pub target_notional_usd: u64,
}

/// Pure: compute per-asset target hedge notionals given the current
/// `delta` and the assignment's `target_delta_bps`. Pro-rata splits
/// total hedge across SOL/ETH/BTC by current long exposure, exactly
/// matching `hedge::allocate_per_asset` (re-implemented here over
/// public types so the resize path doesn't need a `DispatchCtx`).
pub fn compute_per_asset_targets(
    delta: &PortfolioDelta,
    target_delta_bps: i16,
) -> Vec<PerAssetTarget> {
    let total_i128 = delta.total_usd as i128;
    let bps = target_delta_bps as i128;

    let target_net_long_usd_signed = total_i128.saturating_mul(bps) / 10_000;
    let target_net_long_usd: u64 = target_net_long_usd_signed.max(0).min(u64::MAX as i128) as u64;
    let target_net_short_usd: u64 =
        (-target_net_long_usd_signed).max(0).min(u64::MAX as i128) as u64;

    let current_long_usd = delta
        .sol_usd
        .saturating_add(delta.eth_usd)
        .saturating_add(delta.btc_usd);

    let total_hedge_short_usd = current_long_usd
        .saturating_sub(target_net_long_usd)
        .saturating_add(target_net_short_usd);

    if current_long_usd == 0 || total_hedge_short_usd == 0 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(3);
    for (label, mint, usd) in [
        ("SOL", WSOL_MINT, delta.sol_usd),
        ("ETH", WETH_PORTAL_MINT, delta.eth_usd),
        ("BTC", WBTC_PORTAL_MINT, delta.btc_usd),
    ] {
        if usd == 0 {
            continue;
        }
        let share = ((total_hedge_short_usd as u128).saturating_mul(usd as u128)
            / current_long_usd as u128) as u64;
        out.push(PerAssetTarget {
            label,
            mint,
            target_notional_usd: share,
        });
    }
    out
}

/// Pure: subtract the existing per-asset shorts from the targets and
/// return only the *missing* legs (the "delta to open").
///
/// `existing_per_asset` maps an asset label ("SOL"/"ETH"/"BTC") to its
/// current short notional in micro-USD — derived by the caller from
/// `position.open_positions` + a per-asset chain read. v0 callers can
/// supply zeros for legs the daemon has no record of (the existing
/// shorts won't be subtracted, which means resize might re-size a leg
/// already opened on chain — that's why we ALSO have the chain-aware
/// `summarise_existing_shorts_micro_usd` below).
///
/// Honors `cap_total_usdc` as an absolute ceiling on the sum of returned
/// notionals: if the natural sum exceeds the cap, every leg is scaled
/// down proportionally. Returns the shaved-off amount (`cap_hit_usdc`).
///
/// `min_notional` is the floor below which a per-asset leg is dropped
/// (with `SkipReason::BelowMinNotional`).
pub fn compute_legs_to_open(
    targets: &[PerAssetTarget],
    existing_per_asset: &[(&str, u64)],
    cap_total_usdc: u64,
    min_notional: u64,
) -> (Vec<(String, u64)>, Vec<(String, SkipReason)>, Option<u64>) {
    let queued: Vec<(String, u64)>;
    let mut skipped: Vec<(String, SkipReason)> = Vec::new();

    // Step 1: subtract.
    let mut raw: Vec<(String, u64)> = Vec::with_capacity(targets.len());
    for t in targets {
        let current = existing_per_asset
            .iter()
            .find(|(l, _)| *l == t.label)
            .map(|(_, v)| *v)
            .unwrap_or(0);
        if t.target_notional_usd == 0 {
            skipped.push((t.label.to_string(), SkipReason::ZeroExposure));
            continue;
        }
        if current >= t.target_notional_usd {
            skipped.push((t.label.to_string(), SkipReason::AlreadyHedged));
            continue;
        }
        let to_open = t.target_notional_usd.saturating_sub(current);
        if to_open < min_notional {
            skipped.push((t.label.to_string(), SkipReason::BelowMinNotional));
            continue;
        }
        raw.push((t.label.to_string(), to_open));
    }

    // Step 2: cap-down if the sum exceeds cap_total_usdc.
    let sum: u128 = raw.iter().map(|(_, v)| *v as u128).sum();
    let cap_hit_usdc = if sum > cap_total_usdc as u128 {
        let overflow = (sum - cap_total_usdc as u128) as u64;
        // Scale down each leg proportionally. Use u128 math to avoid
        // precision loss; clamp each leg at min_notional (drop if it
        // falls below).
        let mut scaled: Vec<(String, u64)> = Vec::with_capacity(raw.len());
        for (label, v) in raw {
            let new_v = ((v as u128).saturating_mul(cap_total_usdc as u128) / sum) as u64;
            if new_v < min_notional {
                skipped.push((label, SkipReason::BelowMinNotional));
                continue;
            }
            scaled.push((label, new_v));
        }
        queued = scaled;
        Some(overflow)
    } else {
        queued = raw;
        None
    };

    (queued, skipped, cap_hit_usdc)
}

/// Pure: gate a queued resize plan against the wallet's free USDC. Each
/// leg requires `notional_usd / RESIZE_LEVERAGE` USDC for collateral
/// (Jupiter Perps' `Transfer` ixn pulls exactly that amount out of the
/// caller's USDC ATA). When the sum of required collateral exceeds
/// `free_usdc_lamports`, every leg is scaled down proportionally; legs
/// that fall below `min_notional` after scaling are dropped with
/// `SkipReason::InsufficientUsdcLiquidity`.
///
/// Returns `(scaled_legs, additional_skips)`. The caller appends the
/// additional skips to its existing skip list — they're typed
/// distinctly so the audit log can tell "cap-shaved" from "liquidity-
/// shaved" outcomes apart.
///
/// fleet-v0.4.0-rc7: this helper was added after the first on-chain
/// rebalance attempt failed inside Jupiter Perps' SPL Token Transfer
/// with `custom program error: 0x1` (InsufficientFunds). The daemon
/// had $23.07 free USDC in its ATA (other funds were locked in Kamino
/// reserves) and tried to fund an ETH-leg open requiring more than
/// that. We now compute the per-leg collateral requirement off-chain,
/// scale down or skip before signing, and never invoke the program
/// with a known-doomed Transfer.
pub fn gate_legs_by_free_usdc(
    legs: &[(String, u64)],
    free_usdc_lamports: u64,
    leverage: u64,
    min_notional: u64,
) -> (Vec<(String, u64)>, Vec<(String, SkipReason)>) {
    if legs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let lev = leverage.max(1);
    // Total collateral required by the plan as-queued.
    let required_total: u128 = legs
        .iter()
        .map(|(_, notional)| (*notional as u128) / lev as u128)
        .sum();
    let budget = free_usdc_lamports as u128;

    if required_total <= budget {
        // Plan fits inside free USDC — pass-through. Empty skip list.
        return (legs.to_vec(), Vec::new());
    }

    if budget == 0 {
        // Wallet has no free USDC at all. Every leg is skipped with
        // InsufficientUsdcLiquidity so the audit log surfaces WHY.
        let skips = legs
            .iter()
            .map(|(label, _)| (label.clone(), SkipReason::InsufficientUsdcLiquidity))
            .collect();
        return (Vec::new(), skips);
    }

    // Proportional scale-down: each leg's new notional shrinks by the
    // same ratio so allocations stay pro-rata. A leg that lands below
    // `min_notional` after scaling is dropped (its capital frees up,
    // but we don't re-distribute — the rebalancer will reconcile on
    // the next tick).
    //
    // new_notional_i = leverage * (budget * required_i_collateral / required_total)
    //                = old_notional_i * budget / required_total
    let mut scaled: Vec<(String, u64)> = Vec::with_capacity(legs.len());
    let mut skips: Vec<(String, SkipReason)> = Vec::new();
    for (label, notional) in legs {
        let new_notional = ((*notional as u128).saturating_mul(budget) / required_total) as u64;
        if new_notional < min_notional {
            skips.push((label.clone(), SkipReason::InsufficientUsdcLiquidity));
            continue;
        }
        scaled.push((label.clone(), new_notional));
    }
    (scaled, skips)
}

/// Pure: extract the per-asset hedge notional currently on the books
/// from an `ActivePosition`. Pro-rates `hedge_notional_usdc` across the
/// open-positions labels — this is a best-effort approximation because
/// we don't currently track per-leg notional separately. When the open
/// path is single-leg (one entry in `open_positions`), all
/// `hedge_notional_usdc` is attributed to that asset; with N entries we
/// split evenly. A future PR can promote `open_positions` to a
/// `(label, pubkey, size_usd)` 3-tuple to make this exact.
///
/// Note: for resize correctness, exact per-leg sizing isn't critical —
/// the on-chain truth is the source. The rebalancer's *next* tick will
/// re-read pool state and reconcile drift again if we over- or under-
/// shoot here.
pub fn summarise_existing_shorts_micro_usd(position: &ActivePosition) -> Vec<(&'static str, u64)> {
    if position.open_positions.is_empty() {
        return Vec::new();
    }
    let n = position.open_positions.len() as u64;
    let per_leg = position.hedge_notional_usdc / n.max(1);
    let mut out: Vec<(&'static str, u64)> = Vec::with_capacity(position.open_positions.len());
    for (label, _pk) in &position.open_positions {
        let s: &'static str = match label.as_str() {
            "SOL" => "SOL",
            "ETH" => "ETH",
            "BTC" => "BTC",
            _ => continue,
        };
        out.push((s, per_leg));
    }
    out
}

/// Entry point invoked by the rebalancer tick when drift exceeds
/// `MAX_DELTA_DRIFT_BPS`. Computes the plan, validates against caps,
/// enqueues for approval, emits a `NeedsApproval` Escalate envelope.
///
/// Returns `ResizeOutcome` so the rebalancer can structured-log every
/// decision. Errors only on fatal failures (envelope-build or queue-full)
/// — empty plans, caps clamping all legs below min, and full-queue
/// rejections are all returned as `Ok(outcome)`.
pub async fn run_resize(
    ctx: &ResizeCtx,
    position: &ActivePosition,
    delta: &PortfolioDelta,
) -> Result<ResizeOutcome> {
    let targets = compute_per_asset_targets(delta, position.target_delta_bps);
    if targets.is_empty() {
        info!(
            ?position.conv,
            "resize: no per-asset targets (zero long or zero hedge — nothing to do)"
        );
        return Ok(ResizeOutcome::empty());
    }

    let existing = summarise_existing_shorts_micro_usd(position);
    let (cap_queued, mut skipped, cap_hit_usdc) = compute_legs_to_open(
        &targets,
        &existing.iter().map(|(l, v)| (*l, *v)).collect::<Vec<_>>(),
        MAX_POSITION_USDC_LAMPORTS,
        MIN_HEDGE_NOTIONAL_USD,
    );

    // fleet-v0.4.0-rc7: gate the cap-clamped plan against free USDC at
    // queue time too. The execute path runs the gate again right before
    // signing (chain state may move between queue + approve), but
    // surfacing the constraint here gives the operator a clean WARN at
    // the time we ask them to Approve — instead of a surprise "leg
    // dropped at execute" log minutes later. Sim-only runs skip the
    // gate (no wallet to gate against).
    let queued: Vec<(String, u64)> = if ctx.simulate_only || cap_queued.is_empty() {
        cap_queued
    } else {
        let raw_free_opt: Option<u64> =
            match crate::prices::fetch_wallet_free_usdc_lamports(&ctx.rpc, &ctx.wallet.pubkey())
                .await
            {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!(
                        ?position.conv,
                        ?e,
                        "resize: fetch_wallet_free_usdc_lamports errored at queue time — \
                         proceeding with cap-only plan (execute will re-check)"
                    );
                    None
                }
            };
        match raw_free_opt {
            None => cap_queued, // RPC blip: defer to execute-time gate.
            Some(raw_free) => {
                let spendable = raw_free.saturating_sub(crate::prices::USDC_RESERVE_LAMPORTS);
                let (gated, liquidity_skips) = gate_legs_by_free_usdc(
                    &cap_queued,
                    spendable,
                    RESIZE_LEVERAGE,
                    MIN_HEDGE_NOTIONAL_USD,
                );
                if !liquidity_skips.is_empty() || gated.len() != cap_queued.len() {
                    let need: u128 = cap_queued
                        .iter()
                        .map(|(_, n)| (*n as u128) / RESIZE_LEVERAGE as u128)
                        .sum();
                    warn!(
                        ?position.conv,
                        have_usdc_lamports = raw_free,
                        spendable_usdc_lamports = spendable,
                        need_usdc_lamports = need as u64,
                        pre_gate_legs = cap_queued.len(),
                        post_gate_legs = gated.len(),
                        "queue-time pre-flight: insufficient USDC for full resize; \
                         plan scaled or partially skipped"
                    );
                }
                for (label, reason) in liquidity_skips.into_iter() {
                    skipped.push((label, reason));
                }
                gated
            }
        }
    };

    if queued.is_empty() {
        info!(
            ?position.conv,
            skipped_count = skipped.len(),
            "resize: no legs to open — drift either dust, already hedged, or capped-out"
        );
        return Ok(ResizeOutcome {
            queued,
            skipped,
            cap_hit_usdc,
            queued_to_approval: false,
        });
    }

    // Cap re-check on every leg's borrow rate hint — the position's
    // recorded `max_borrow_rate_bps` already passed `caps::validate_assign`
    // at Assign time, but defense-in-depth: refuse to enqueue if a
    // future cap relaxation has been bypassed.
    if position.max_borrow_rate_bps > MAX_BORROW_RATE_BPS_HARDCAP {
        warn!(
            ?position.conv,
            stored = position.max_borrow_rate_bps,
            cap = MAX_BORROW_RATE_BPS_HARDCAP,
            "resize: position's recorded max_borrow_rate_bps exceeds hard cap — refusing to enqueue"
        );
        return Ok(ResizeOutcome {
            queued: vec![],
            skipped,
            cap_hit_usdc,
            queued_to_approval: false,
        });
    }

    // Compute observed drift for the audit trail.
    let observed_drift_bps = crate::rebalance::compute_drift_bps(delta, position.target_delta_bps);

    let plan = ResizePlan {
        legs: queued.clone(),
        observed_drift_bps,
        target_delta_bps: position.target_delta_bps,
    };

    // The sender we record on the queue is the orchestrator pubkey
    // (when configured) so the matching `Approve` envelope must come
    // from the orchestrator — same audit-fix C1 shape as Assign /
    // Withdraw. When the allowlist is disabled (devnet sandbox), use a
    // zero-pubkey placeholder; the `Approve` handler in `dispatch.rs`
    // already skips the sender-match check when the orchestrator
    // allowlist is `None`.
    let queued_sender = ctx.orchestrator_agent_id.unwrap_or([0u8; 32]);

    // When `require_approval=false` the operator has opted out of the
    // orchestrator-Approve gate — execute the plan immediately (same as
    // if an Approve envelope had already arrived). This unblocks the
    // rebalancer when the orchestrator's nonce-replay protection would
    // otherwise permanently reject every NeedsApproval Escalate from a
    // freshly restarted daemon (nonce counter resets to 1 on restart).
    if !ctx.require_approval {
        info!(
            ?position.conv,
            queued_count = queued.len(),
            skipped_count = skipped.len(),
            ?cap_hit_usdc,
            observed_drift_bps,
            "require_approval=false: executing resize plan immediately (no orchestrator Approve needed)"
        );
        let conv = position.conv;
        match execute_resize(ctx, &plan, conv).await {
            Ok(sigs) => {
                info!(?conv, sig_count = sigs.len(), "resize auto-executed successfully");
            }
            Err(e) => {
                warn!(?e, ?conv, "resize auto-execute failed — will retry next tick");
            }
        }
        return Ok(ResizeOutcome {
            queued,
            skipped,
            cap_hit_usdc,
            queued_to_approval: false,
        });
    }

    let added = ctx.resize_queue.enqueue(position.conv, plan, queued_sender);
    if !added {
        warn!(
            ?position.conv,
            "resize: approval queue full (cap 64); not emitting NeedsApproval"
        );
        return Ok(ResizeOutcome {
            queued,
            skipped,
            cap_hit_usdc,
            queued_to_approval: false,
        });
    }

    if let Err(e) = emit_needs_approval(&ctx.handle, &ctx.role, &ctx.nonce, position.conv).await {
        warn!(
            ?e,
            ?position.conv,
            "resize: failed to emit NeedsApproval Escalate; plan still queued"
        );
    }

    info!(
        ?position.conv,
        queued_count = queued.len(),
        skipped_count = skipped.len(),
        ?cap_hit_usdc,
        observed_drift_bps,
        "resize plan queued for approval"
    );

    Ok(ResizeOutcome {
        queued,
        skipped,
        cap_hit_usdc,
        queued_to_approval: true,
    })
}

/// Build + send a `NeedsApproval` Escalate envelope. Mirrors
/// `dispatch::emit_needs_approval` but lives here so the rebalancer
/// doesn't need a `DispatchCtx`. Recipient is the broadcast pubkey —
/// the orchestrator filters its inbox by `RiskKind::NeedsApproval`.
async fn emit_needs_approval(
    handle: &NodeHandle,
    role: &RoleIdentity,
    nonce: &Arc<std::sync::atomic::AtomicU64>,
    conv: [u8; 16],
) -> Result<()> {
    let signing_key = ed25519_dalek::SigningKey::from_bytes(role.signing_key_bytes());
    let sender = signing_key.verifying_key().to_bytes();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let payload = EscalateRisk {
        severity: RiskSeverity::Notice,
        kind: RiskKind::NeedsApproval,
        subject: [0u8; 32],
        measurement: 0,
        raised_at_unix: now,
    };
    let mut payload_bytes = Vec::new();
    ciborium::ser::into_writer(&payload, &mut payload_bytes)
        .context("serialize NeedsApproval EscalateRisk")?;

    let n = nonce.fetch_add(1, Ordering::Relaxed);
    let env = Envelope::build(
        MsgType::Escalate,
        sender,
        zerox1_protocol::envelope::BROADCAST_RECIPIENT,
        now,
        n,
        conv,
        payload_bytes,
        &signing_key,
    );
    handle
        .send(env)
        .await
        .context("send NeedsApproval Escalate (resize)")?;
    info!(?conv, "resize NeedsApproval emitted");
    Ok(())
}

/// Execute an approved `ResizePlan`. Called from `dispatch::handle_approve`
/// when an Approve envelope drains the resize queue.
///
/// For each `(label, notional)` leg in the plan:
///   1. Resolve the per-asset + collateral `CustodyMeta` from the live
///      pool (synthetic fallback fires the audit-fix C3 hard-stop on
///      submit mode — same as `hedge::open_short_requests`).
///   2. Derive a fresh `(position, position_request)` PDA pair using
///      `unix_seconds + i` as the counter — same formula `hedge.rs`
///      uses at Assign time.
///   3. Whitelist-verify and submit (or simulate). On submit success,
///      append a new entry to `state.active.open_positions` AND
///      increment `state.active.hedge_notional_usdc`.
///
/// Idempotent on retry: if the operator double-approves and the queue
/// somehow re-emits, the second call's pre-execution
/// `compute_legs_to_open` (run by `run_resize` on the next rebalancer
/// tick that re-detects drift) will see the updated state and return an
/// empty plan. Within a single `execute_resize` call there is no
/// idempotency guard — the dispatch's resize queue consumes the entry
/// on approve, so a stale duplicate cannot re-enter the executor.
pub async fn execute_resize(
    ctx: &ResizeCtx,
    plan: &ResizePlan,
    conv: [u8; 16],
) -> Result<Vec<solana_sdk::signature::Signature>> {
    info!(
        ?conv,
        leg_count = plan.legs.len(),
        target_delta_bps = plan.target_delta_bps,
        observed_drift_bps = plan.observed_drift_bps,
        simulate_only = ctx.simulate_only,
        "executing approved resize plan"
    );

    // Build the live pool meta if we have one. Synthetic fallback
    // tripwire fires inside `validate_custody_not_synthetic`.
    let pool: PoolMeta = match &ctx.pool {
        Some(p) => (**p).clone(),
        None => synthetic_pool(),
    };

    let user = ctx.wallet.pubkey();
    let counter_base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // ── Pre-flight USDC liquidity gate (fleet-v0.4.0-rc7) ──────────────
    //
    // Jupiter Perps' `create_increase_position_market_request` invokes
    // an SPL Token `Transfer` that pulls `collateral_amount = notional /
    // leverage` USDC out of the wallet's USDC ATA. An under-funded ATA
    // produces a 1200-line `custom program error: 0x1` (Token::
    // InsufficientFunds) inside the program log — opaque, slow to
    // diagnose, and a wasted RPC round-trip. Skip-or-scale before
    // signing so the audit log surfaces a clean
    // `SkipReason::InsufficientUsdcLiquidity` instead.
    //
    // Free USDC = wallet's USDC ATA balance only. Funds inside Kamino
    // reserves / JLP pool / other venues are NOT spendable for
    // collateral. The reserve constant (`USDC_RESERVE_LAMPORTS`)
    // shaves off ~$1 for tx fees + ATA rent before declaring the rest
    // available.
    //
    // simulate_only=true skips the gate — sim runs are informational
    // and the operator may want to exercise the path on a wallet with
    // zero free USDC (devnet, fresh test).
    let executable_legs: Vec<(String, u64)>;
    let liquidity_skips: Vec<(String, SkipReason)>;
    if ctx.simulate_only {
        executable_legs = plan.legs.clone();
        liquidity_skips = Vec::new();
    } else {
        let raw_free = match crate::prices::fetch_wallet_free_usdc_lamports(&ctx.rpc, &user).await {
            Ok(v) => v,
            Err(e) => {
                warn!(?conv, ?e, "resize: fetch_wallet_free_usdc_lamports errored — skipping execution as a safety net");
                return Ok(Vec::new());
            }
        };
        let spendable = raw_free.saturating_sub(crate::prices::USDC_RESERVE_LAMPORTS);
        let required_total: u128 = plan
            .legs
            .iter()
            .map(|(_, n)| (*n as u128) / RESIZE_LEVERAGE as u128)
            .sum();
        let (gated, skips) = gate_legs_by_free_usdc(
            &plan.legs,
            spendable,
            RESIZE_LEVERAGE,
            crate::hedge::MIN_HEDGE_NOTIONAL_USD,
        );
        if !skips.is_empty() || gated.len() != plan.legs.len() {
            warn!(
                ?conv,
                have_usdc_lamports = raw_free,
                spendable_usdc_lamports = spendable,
                need_usdc_lamports = required_total as u64,
                planned_legs = plan.legs.len(),
                gated_legs = gated.len(),
                skipped_legs = skips.len(),
                "insufficient USDC for full resize: scaling/skipping to fit wallet ATA balance"
            );
        } else {
            info!(
                ?conv,
                spendable_usdc_lamports = spendable,
                need_usdc_lamports = required_total as u64,
                "resize pre-flight USDC check passed"
            );
        }
        executable_legs = gated;
        liquidity_skips = skips;
    }
    // Surface every leg dropped by the liquidity gate as a WARN so the
    // operator's audit log records the reason — never a silent skip.
    for (label, reason) in &liquidity_skips {
        warn!(?conv, label = %label, ?reason, "resize: leg dropped by pre-flight USDC liquidity gate");
    }

    // Fetch live oracle prices for all three asset mints before the open loop.
    // Same fix as hedge::open_short_requests — stale prices caused the ETH
    // floor to be set above the oracle, blocking all ETH keeper fills.
    let price_mints = [WSOL_MINT, WETH_PORTAL_MINT, WBTC_PORTAL_MINT];
    let live_prices = crate::prices::fetch_custody_prices_micro_usd(&price_mints)
        .await
        .unwrap_or_default();

    let mut signatures = Vec::new();
    let mut newly_opened: Vec<(String, Pubkey)> = Vec::new();
    let mut total_opened_usdc: u64 = 0;

    for (i, (label, notional_usd)) in executable_legs.iter().enumerate() {
        let asset_mint = match label.as_str() {
            "SOL" => WSOL_MINT,
            "ETH" => WETH_PORTAL_MINT,
            "BTC" => WBTC_PORTAL_MINT,
            other => {
                warn!(?conv, label = %other, "resize: unknown asset label; skipping leg");
                continue;
            }
        };
        let position_custody = pool
            .custody_for_mint(&asset_mint)
            .cloned()
            .unwrap_or_else(|| synthetic_custody(asset_mint, false));
        let collateral_custody = pool
            .custody_for_mint(&USDC_MINT)
            .cloned()
            .unwrap_or_else(|| synthetic_custody(USDC_MINT, true));

        if let Err(e) = validate_custody_not_synthetic(
            &position_custody,
            &format!("resize open ({label}) position-custody"),
            ctx.simulate_only,
        ) {
            warn!(?conv, label = %label, ?e, "resize: synthetic custody hard-stop on submit");
            continue;
        }
        if let Err(e) = validate_custody_not_synthetic(
            &collateral_custody,
            &format!("resize open ({label}) collateral-custody"),
            ctx.simulate_only,
        ) {
            warn!(?conv, label = %label, ?e, "resize: synthetic custody hard-stop on submit");
            continue;
        }

        let counter = counter_base.wrapping_add(i as u64);
        let position = derive_position(
            &user,
            &pool.pool,
            &position_custody.address,
            &collateral_custody.address,
            PerpSide::Short,
        );
        let position_request = derive_position_request(&position, counter, RequestChange::Increase);

        let collateral_amount = *notional_usd / RESIZE_LEVERAGE;
        let asset_mint_for_price = match label.as_str() {
            "SOL" => WSOL_MINT,
            "ETH" => WETH_PORTAL_MINT,
            "BTC" => WBTC_PORTAL_MINT,
            _ => WSOL_MINT,
        };
        let live_mark = live_prices
            .get(&asset_mint_for_price)
            .copied()
            .map(|p| p as u64)
            .unwrap_or_else(|| crate::hedge::sim_mark_price_micro_usd(label));
        info!(
            ?conv,
            label = %label,
            live_mark_usd = live_mark / 1_000_000,
            "resize: using live oracle price for slippage floor"
        );
        let price_slippage_micro_usd = crate::hedge::short_price_floor_micro_usd(live_mark);

        let ixs = match create_increase_position_request_ix(
            &user,
            &pool,
            &position_custody,
            &collateral_custody,
            &position,
            &position_request,
            *notional_usd,
            collateral_amount,
            PerpSide::Short,
            price_slippage_micro_usd,
            counter,
        ) {
            Ok(v) => v,
            Err(e) => {
                warn!(?conv, label = %label, ?e, "resize: build_increase ix failed");
                continue;
            }
        };

        if let Err(e) = ctx.whitelist.verify_ixns(&ixs) {
            warn!(?conv, label = %label, ?e, "resize: whitelist rejected ixns");
            continue;
        }

        if ctx.simulate_only {
            match ctx
                .rpc
                .build_sign_simulate(
                    ixs,
                    ctx.wallet.keypair(),
                    RESIZE_CU_LIMIT,
                    RESIZE_PRIORITY_FEE,
                )
                .await
            {
                Ok(sim) => {
                    let (layout_valid, summary) = classify_simulation(&sim);
                    if sim.err.is_some() {
                        warn!(
                            ?conv,
                            label = %label,
                            layout_valid,
                            summary = %summary,
                            err = ?sim.err,
                            "resize: simulation returned error (expected on devnet)"
                        );
                    } else {
                        info!(?conv, label = %label, layout_valid, summary = %summary, "resize: simulation ok");
                    }
                    // Sim-only never persists into state (mirrors the
                    // audit-fix C1 invariant from `jlp_hedge.rs`).
                }
                Err(e) => warn!(?conv, label = %label, ?e, "resize: build_sign_simulate threw"),
            }
        } else {
            match ctx
                .rpc
                .build_sign_send(
                    ixs,
                    ctx.wallet.keypair(),
                    RESIZE_CU_LIMIT,
                    RESIZE_PRIORITY_FEE,
                )
                .await
            {
                Ok(sig) => {
                    info!(?conv, label = %label, %sig, "resize: short-open request submitted");
                    match crate::hedge::wait_for_nonzero_position_size(
                        &ctx.rpc,
                        position,
                        label,
                        RESIZE_FILL_VERIFY_ATTEMPTS,
                        RESIZE_FILL_VERIFY_DELAY,
                    )
                    .await
                    {
                        Some(size_usd) => {
                            signatures.push(sig);
                            // Persist only keeper-executed fills. The
                            // request transaction alone can leave an
                            // empty Position PDA when keeper execution
                            // rejects.
                            newly_opened.push((label.clone(), position));
                            total_opened_usdc = total_opened_usdc.saturating_add(size_usd);
                            info!(
                                ?conv,
                                label = %label,
                                position = %position,
                                size_usd,
                                "resize: keeper fill verified on-chain"
                            );
                        }
                        None => {
                            warn!(
                                ?conv,
                                label = %label,
                                position = %position,
                                %sig,
                                requested_notional_usd = *notional_usd,
                                "resize request submitted but keeper fill not verified; \
                                 not recording leg as open"
                            );
                        }
                    }
                }
                Err(e) => warn!(?conv, label = %label, ?e, "resize: build_sign_send failed"),
            }
        }
    }

    // Persist newly-opened legs into the active position. Sim-only
    // calls don't touch state (no on-chain truth to mirror) — same
    // invariant as `jlp_hedge::run_or_simulate`.
    if !ctx.simulate_only && !newly_opened.is_empty() {
        let mut guard = ctx.state.active.lock().expect("active poisoned");
        if let Some(active) = guard.as_mut() {
            for entry in newly_opened {
                active.open_positions.push(entry);
            }
            active.hedge_notional_usdc =
                active.hedge_notional_usdc.saturating_add(total_opened_usdc);
            info!(
                ?conv,
                new_hedge_notional_usdc = active.hedge_notional_usdc,
                open_positions = active.open_positions.len(),
                "resize: active position updated with newly-opened legs"
            );
        } else {
            // The active slot was cleared between approval and execute
            // (a Withdraw in flight, or a daemon-internal race). Don't
            // re-populate it; let the next boot's `recover.rs` rebuild
            // from chain.
            warn!(
                ?conv,
                "resize: active position cleared during execute; not re-populating state"
            );
        }
    }

    Ok(signatures)
}

// ── Synthetic fallbacks (mirrors hedge.rs) ─────────────────────────────

fn synthetic_pool() -> PoolMeta {
    use zerox1_defi_protocols::constants::{JLP_MINT, JLP_POOL};
    use zerox1_defi_protocols::protocols::jlp::{
        derive_event_authority, derive_perpetuals, derive_transfer_authority,
    };
    PoolMeta {
        pool: JLP_POOL,
        jlp_mint: JLP_MINT,
        perpetuals: derive_perpetuals(),
        transfer_authority: derive_transfer_authority(),
        event_authority: derive_event_authority(),
        custodies: vec![],
    }
}

fn synthetic_custody(
    mint: Pubkey,
    is_stable: bool,
) -> zerox1_defi_protocols::protocols::jlp::CustodyMeta {
    let synth = solana_sdk::pubkey!("G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa");
    zerox1_defi_protocols::protocols::jlp::CustodyMeta {
        address: synth,
        mint,
        token_account: synth,
        pythnet_price_account: synth,
        doves_price_account: synth,
        decimals: if is_stable { 6 } else { 9 },
        is_stable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── compute_per_asset_targets ──────────────────────────────────────

    fn delta_174_dollar_position() -> PortfolioDelta {
        // Mirror the prod incident: $174 JLP, ~57% non-stable (per the
        // pool's target weights). SOL=$77M, BTC=$19M, ETH=$16M = $112M
        // total long; stable bucket = $62M.
        PortfolioDelta {
            sol_usd: 77_000_000,
            eth_usd: 16_000_000,
            btc_usd: 19_000_000,
            stable_usd: 62_000_000,
            total_usd: 174_000_000,
            long_exposure_bps: 6_437,
        }
    }

    #[test]
    fn targets_for_174_position_split_pro_rata() {
        // target_delta_bps = 0 → fully neutralize current long ($112M)
        // SOL=77, ETH=16, BTC=19 → total 112
        let d = delta_174_dollar_position();
        let t = compute_per_asset_targets(&d, 0);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].label, "SOL");
        assert_eq!(t[1].label, "ETH");
        assert_eq!(t[2].label, "BTC");
        // Sums to current long ± rounding.
        let sum: u64 = t.iter().map(|x| x.target_notional_usd).sum();
        assert!(
            sum >= 111_999_990 && sum <= 112_000_010,
            "pro-rata sum should be ≈ $112M (got {sum})"
        );
    }

    #[test]
    fn targets_empty_when_no_long() {
        let mut d = delta_174_dollar_position();
        d.sol_usd = 0;
        d.eth_usd = 0;
        d.btc_usd = 0;
        assert!(compute_per_asset_targets(&d, 0).is_empty());
    }

    // ── compute_legs_to_open (the prod-bug coverage) ────────────────────

    fn three_targets(sol: u64, eth: u64, btc: u64) -> Vec<PerAssetTarget> {
        vec![
            PerAssetTarget {
                label: "SOL",
                mint: WSOL_MINT,
                target_notional_usd: sol,
            },
            PerAssetTarget {
                label: "ETH",
                mint: WETH_PORTAL_MINT,
                target_notional_usd: eth,
            },
            PerAssetTarget {
                label: "BTC",
                mint: WBTC_PORTAL_MINT,
                target_notional_usd: btc,
            },
        ]
    }

    #[test]
    fn nothing_needed_when_existing_meets_target() {
        // Idempotency case: existing == target → all skipped as
        // `AlreadyHedged`, queue empty.
        let targets = three_targets(77_000_000, 16_000_000, 19_000_000);
        let existing = vec![
            ("SOL", 77_000_000),
            ("ETH", 16_000_000),
            ("BTC", 19_000_000),
        ];
        let (q, s, cap) = compute_legs_to_open(
            &targets,
            &existing,
            MAX_POSITION_USDC_LAMPORTS,
            MIN_HEDGE_NOTIONAL_USD,
        );
        assert!(q.is_empty(), "no legs to open when fully hedged");
        assert_eq!(s.len(), 3);
        for (_, reason) in &s {
            assert_eq!(*reason, SkipReason::AlreadyHedged);
        }
        assert!(cap.is_none());
    }

    #[test]
    fn prod_174_case_btc_present_sol_eth_missing() {
        // The exact reason this PR exists: $174 JLP, only BTC short was
        // opened ($18M of $19M target). SOL + ETH legs were never
        // re-opened after a partial Assign failure. Resize must
        // generate two legs: SOL ≈$77M, ETH ≈$16M; BTC = already
        // hedged.
        let targets = three_targets(77_000_000, 16_000_000, 19_000_000);
        let existing = vec![("BTC", 18_000_000)];
        let (q, s, cap) = compute_legs_to_open(
            &targets,
            &existing,
            MAX_POSITION_USDC_LAMPORTS,
            MIN_HEDGE_NOTIONAL_USD,
        );
        assert_eq!(q.len(), 2, "SOL + ETH legs needed; BTC partially hedged");
        let labels: Vec<&str> = q.iter().map(|(l, _)| l.as_str()).collect();
        assert!(labels.contains(&"SOL"));
        assert!(labels.contains(&"ETH"));
        // BTC: target=19M, current=18M → delta=1M < MIN_HEDGE_NOTIONAL_USD (10M)
        // so it's skipped as BelowMinNotional (not AlreadyHedged).
        let btc_skip = s.iter().find(|(l, _)| l == "BTC").expect("btc skipped");
        assert_eq!(btc_skip.1, SkipReason::BelowMinNotional);
        assert!(cap.is_none());
    }

    #[test]
    fn all_three_missing_returns_three_legs() {
        let targets = three_targets(77_000_000, 16_000_000, 19_000_000);
        let existing: Vec<(&str, u64)> = vec![];
        let (q, _s, _cap) = compute_legs_to_open(
            &targets,
            &existing,
            MAX_POSITION_USDC_LAMPORTS,
            MIN_HEDGE_NOTIONAL_USD,
        );
        assert_eq!(q.len(), 3);
        // Order is preserved from the targets list.
        assert_eq!(q[0].0, "SOL");
        assert_eq!(q[0].1, 77_000_000);
        assert_eq!(q[1].0, "ETH");
        assert_eq!(q[1].1, 16_000_000);
        assert_eq!(q[2].0, "BTC");
        assert_eq!(q[2].1, 19_000_000);
    }

    #[test]
    fn overshoot_prevented_when_existing_exceeds_target() {
        // Target is below existing — that asset is `AlreadyHedged`,
        // delta is NOT negative (no "close-to-resize" path).
        let targets = three_targets(50_000_000, 16_000_000, 19_000_000);
        let existing = vec![("SOL", 80_000_000)]; // 30M over target
        let (q, s, _cap) = compute_legs_to_open(
            &targets,
            &existing,
            MAX_POSITION_USDC_LAMPORTS,
            MIN_HEDGE_NOTIONAL_USD,
        );
        // SOL: skip; ETH + BTC: open. No "close 30M of SOL" leg.
        let sol_skip = s.iter().find(|(l, _)| l == "SOL").expect("sol skipped");
        assert_eq!(sol_skip.1, SkipReason::AlreadyHedged);
        assert_eq!(
            q.len(),
            2,
            "ETH + BTC opens; SOL skipped (no over-hedge close)"
        );
        for (l, _) in &q {
            assert_ne!(l, "SOL", "SOL must not appear in queued legs");
        }
    }

    #[test]
    fn respects_max_position_cap() {
        // Target sum exceeds cap → every leg scaled down proportionally.
        let cap = 30_000_000u64; // $30M cap
        let targets = three_targets(60_000_000, 30_000_000, 30_000_000);
        let (q, _s, cap_hit) = compute_legs_to_open(&targets, &[], cap, MIN_HEDGE_NOTIONAL_USD);
        let sum: u64 = q.iter().map(|(_, v)| *v).sum();
        assert!(
            sum <= cap + 1,
            "queued sum {sum} must be at or below cap {cap}"
        );
        // cap_hit_usdc reports the shaved-off amount = 120M - 30M = 90M
        assert_eq!(cap_hit, Some(90_000_000));
    }

    #[test]
    fn below_min_dropped() {
        // Single asset with delta below MIN → skipped.
        let targets = vec![PerAssetTarget {
            label: "SOL",
            mint: WSOL_MINT,
            target_notional_usd: 5_000_000, // $5 = below $10 MIN
        }];
        let (q, s, _cap) = compute_legs_to_open(
            &targets,
            &[],
            MAX_POSITION_USDC_LAMPORTS,
            MIN_HEDGE_NOTIONAL_USD,
        );
        assert!(q.is_empty());
        assert_eq!(s[0].1, SkipReason::BelowMinNotional);
    }

    #[test]
    fn summarise_existing_shorts_micro_usd_pro_rates_evenly() {
        let pk = Pubkey::new_unique();
        let position = ActivePosition {
            conv: [0u8; 16],
            our_jlp_lamports: 100,
            jlp_acquired_lamports: 100,
            target_delta_bps: 0,
            max_borrow_rate_bps: 5_000,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 90_000_000,
            open_positions: vec![
                ("SOL".to_string(), pk),
                ("ETH".to_string(), pk),
                ("BTC".to_string(), pk),
            ],
        };
        let s = summarise_existing_shorts_micro_usd(&position);
        assert_eq!(s.len(), 3);
        for (_, v) in &s {
            assert_eq!(*v, 30_000_000, "even split: 90M / 3 = 30M");
        }
    }

    #[test]
    fn idempotent_no_op_when_target_met() {
        // Pure-pipeline idempotency: given an `existing` snapshot that
        // already meets per-asset targets, compute_legs_to_open returns
        // an empty queue. This is the second-Approve case — by the time
        // the rebalancer re-evaluates, state.active reflects the legs
        // opened on the first Approve, and the resize is a no-op.
        let targets = three_targets(77_000_000, 16_000_000, 19_000_000);
        // Existing exactly meets target.
        let existing = vec![
            ("SOL", 77_000_000),
            ("ETH", 16_000_000),
            ("BTC", 19_000_000),
        ];
        let (q, _s, _cap) = compute_legs_to_open(
            &targets,
            &existing,
            MAX_POSITION_USDC_LAMPORTS,
            MIN_HEDGE_NOTIONAL_USD,
        );
        assert!(
            q.is_empty(),
            "fully-hedged → empty resize plan (idempotent)"
        );
        // And once more for good measure — running the function a
        // second time with the same inputs is also a no-op.
        let (q2, _s2, _cap2) = compute_legs_to_open(
            &targets,
            &existing,
            MAX_POSITION_USDC_LAMPORTS,
            MIN_HEDGE_NOTIONAL_USD,
        );
        assert!(q2.is_empty(), "second pass still no-op");
    }

    // ── async pipeline smoke ───────────────────────────────────────────
    //
    // We cannot stand up a real `NodeHandle` inside a unit test without
    // spinning up an embedded `NodeService` (which would do libp2p
    // discovery). Instead, the async smoke exercises the pure pipeline
    // verbatim — the same call sequence `run_resize` walks before any
    // network-effecting work — and asserts the outcome shape. This is
    // sufficient to catch a regression where the rebalancer would
    // queue duplicate or over-hedge legs.

    /// End-to-end-shaped test that exercises the pure pipeline with a
    /// fully-hedged position. Builds the same path `tick_once` walks:
    /// compute_per_asset_targets → summarise → compute_legs_to_open
    /// and asserts the empty-plan branch surfaces correctly.
    #[tokio::test]
    async fn pipeline_with_empty_plan_returns_outcome_not_queued() {
        // Exercises the same call sequence `run_resize` walks before
        // any network-effecting work, with a position whose per-asset
        // existing shorts (a) are above every per-asset target so all
        // three assets land in `AlreadyHedged`. We can't construct a
        // real `NodeHandle` in a unit test (would require spinning up
        // libp2p discovery), so the test stops at the pre-emit step —
        // sufficient to lock down the empty-plan branch shape.
        let pk = Pubkey::new_unique();
        let delta = delta_174_dollar_position();
        // Over-hedged in every bucket: each leg holds $100M, target
        // per-asset is at most $77M (SOL) — every leg is AlreadyHedged.
        let position = ActivePosition {
            conv: [1u8; 16],
            our_jlp_lamports: 100,
            jlp_acquired_lamports: 100,
            target_delta_bps: 0,
            max_borrow_rate_bps: 5_000,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 300_000_000, // 3 × $100M
            open_positions: vec![
                ("SOL".to_string(), pk),
                ("ETH".to_string(), pk),
                ("BTC".to_string(), pk),
            ],
        };
        let targets = compute_per_asset_targets(&delta, position.target_delta_bps);
        let existing = summarise_existing_shorts_micro_usd(&position);
        let existing_refs: Vec<(&str, u64)> = existing.iter().map(|(l, v)| (*l, *v)).collect();
        let (q, s, cap_hit) = compute_legs_to_open(
            &targets,
            &existing_refs,
            MAX_POSITION_USDC_LAMPORTS,
            MIN_HEDGE_NOTIONAL_USD,
        );
        assert!(q.is_empty(), "over-hedged position queues nothing");
        assert_eq!(s.len(), 3, "all three assets skipped");
        for (_, reason) in &s {
            assert_eq!(*reason, SkipReason::AlreadyHedged);
        }
        assert!(cap_hit.is_none());
    }

    // ── plan serialisation (so the queue can round-trip a clone) ───────

    #[test]
    fn resize_plan_round_trips_through_cbor() {
        let plan = ResizePlan {
            legs: vec![
                ("SOL".to_string(), 77_000_000),
                ("ETH".to_string(), 16_000_000),
            ],
            observed_drift_bps: 2_500,
            target_delta_bps: 0,
        };
        let mut buf = Vec::new();
        ciborium::ser::into_writer(&plan, &mut buf).expect("serialize");
        let back: ResizePlan = ciborium::de::from_reader(&buf[..]).expect("deserialize");
        assert_eq!(back, plan);
    }

    // ── Bug C: pre-flight USDC liquidity gate ───────────────────────────
    //
    // The fleet-v0.4.0-rc7 incident: on the first on-chain rebalance
    // attempt, `execute_resize` optimistically signed an ETH-leg open
    // against a wallet with $23.07 free USDC — insufficient for the
    // leg's collateral requirement. Jupiter Perps' SPL Token Transfer
    // returned `custom program error: 0x1` (InsufficientFunds) deep
    // inside a 1200-line program log. These tests pin the gate that
    // catches that case off-chain.

    #[test]
    fn pre_flight_usdc_check_passes_when_sufficient() {
        // Two legs at $77M + $16M notional, leverage 5x → required
        // collateral = $15.4M + $3.2M = $18.6M total. Wallet has $30M
        // free USDC (well above need). Gate should pass-through with no
        // skips.
        let legs = vec![
            ("SOL".to_string(), 77_000_000u64),
            ("ETH".to_string(), 16_000_000u64),
        ];
        let free_usdc = 30_000_000u64; // $30M
        let (gated, skips) =
            gate_legs_by_free_usdc(&legs, free_usdc, RESIZE_LEVERAGE, MIN_HEDGE_NOTIONAL_USD);
        assert_eq!(gated, legs, "sufficient liquidity must be a pass-through");
        assert!(
            skips.is_empty(),
            "no liquidity skips when budget covers need"
        );
    }

    #[test]
    fn pre_flight_usdc_check_scales_down_when_short() {
        // Two legs requiring $20M total collateral, budget $10M (half).
        // Each leg should scale down by ~50%; both stay above
        // MIN_HEDGE_NOTIONAL_USD ($10M) so neither is dropped.
        //
        // Sizes chosen so post-scale notionals comfortably clear MIN.
        let legs = vec![
            ("SOL".to_string(), 60_000_000u64), // collateral = 12M
            ("ETH".to_string(), 40_000_000u64), // collateral = 8M
        ];
        // total required collateral = 20M; budget = 10M (half).
        let free_usdc = 10_000_000u64;
        let (gated, skips) =
            gate_legs_by_free_usdc(&legs, free_usdc, RESIZE_LEVERAGE, MIN_HEDGE_NOTIONAL_USD);
        assert_eq!(gated.len(), 2, "both legs survive the scale-down");
        assert!(skips.is_empty(), "no skips when scaled legs clear MIN");
        // Post-scale: each leg shrinks by half. New SOL notional = 30M,
        // new ETH notional = 20M; sum-of-collateral = 30M/5 + 20M/5 =
        // 6M + 4M = 10M ≤ budget. Pro-rata preserved.
        let sol = gated.iter().find(|(l, _)| l == "SOL").unwrap().1;
        let eth = gated.iter().find(|(l, _)| l == "ETH").unwrap().1;
        assert_eq!(sol, 30_000_000, "SOL leg scaled to 60M * 10M / 20M = 30M");
        assert_eq!(eth, 20_000_000, "ETH leg scaled to 40M * 10M / 20M = 20M");
        // Verify the post-scale collateral sum fits the budget.
        let post_collateral = sol / RESIZE_LEVERAGE + eth / RESIZE_LEVERAGE;
        assert!(
            post_collateral <= free_usdc,
            "scaled-down collateral {post_collateral} must fit budget {free_usdc}"
        );
    }

    #[test]
    fn pre_flight_usdc_check_skips_when_no_room_for_min_notional() {
        // Two legs each well above MIN_HEDGE_NOTIONAL_USD ($10M), but
        // budget is so tight that the proportional scale-down pushes
        // them BELOW MIN. Both must be skipped with
        // SkipReason::InsufficientUsdcLiquidity, not left in the queue
        // as broken dust.
        let legs = vec![
            ("SOL".to_string(), 50_000_000u64), // collateral = 10M
            ("ETH".to_string(), 50_000_000u64), // collateral = 10M
        ];
        // Total required collateral = 20M. Budget = 2M (1/10th of need).
        // Each leg scales to 5M notional, below MIN ($10M) → dropped.
        let free_usdc = 2_000_000u64;
        let (gated, skips) =
            gate_legs_by_free_usdc(&legs, free_usdc, RESIZE_LEVERAGE, MIN_HEDGE_NOTIONAL_USD);
        assert!(
            gated.is_empty(),
            "both legs must drop below MIN after scale"
        );
        assert_eq!(skips.len(), 2, "both legs reported as liquidity skips");
        for (_, reason) in &skips {
            assert_eq!(*reason, SkipReason::InsufficientUsdcLiquidity);
        }
    }

    #[test]
    fn pre_flight_usdc_check_zero_budget_skips_all() {
        // Edge: wallet has zero free USDC at all. Every leg surfaces
        // as InsufficientUsdcLiquidity — never a silent skip.
        let legs = vec![
            ("SOL".to_string(), 77_000_000u64),
            ("ETH".to_string(), 16_000_000u64),
            ("BTC".to_string(), 19_000_000u64),
        ];
        let (gated, skips) =
            gate_legs_by_free_usdc(&legs, 0u64, RESIZE_LEVERAGE, MIN_HEDGE_NOTIONAL_USD);
        assert!(gated.is_empty());
        assert_eq!(skips.len(), 3);
        for (_, reason) in &skips {
            assert_eq!(*reason, SkipReason::InsufficientUsdcLiquidity);
        }
    }

    #[test]
    fn pre_flight_usdc_check_empty_input_is_noop() {
        // Defensive: empty plan returns empty outputs without dividing
        // by zero on the proportional-scale math.
        let (gated, skips) =
            gate_legs_by_free_usdc(&[], 100_000_000, RESIZE_LEVERAGE, MIN_HEDGE_NOTIONAL_USD);
        assert!(gated.is_empty());
        assert!(skips.is_empty());
    }

    #[test]
    fn pre_flight_usdc_check_matches_prod_incident_shape() {
        // The fleet-v0.4.0-rc7 incident shape: rebalancer planned an
        // ETH leg at $150 notional (large, post-resize). Wallet has
        // $21.07 spendable USDC after reserve. At 5x leverage,
        // collateral required is $30. The gate scales the leg down so
        // the post-scale collateral ($21.07) fits the budget, leaving a
        // single executable leg at ~$105.35 notional. Verifies that
        // the gate's primary job — never sign an under-funded
        // Transfer — holds for the prod-incident shape. The
        // "drops-below-MIN" path is tested separately.
        let legs = vec![("ETH".to_string(), 150_000_000u64)];
        let free_usdc_after_reserve = 22_070_000u64 - 1_000_000u64; // $21.07
        let (gated, skips) = gate_legs_by_free_usdc(
            &legs,
            free_usdc_after_reserve,
            RESIZE_LEVERAGE,
            MIN_HEDGE_NOTIONAL_USD,
        );
        // Leg scales down: 150M * 21_070_000 / 30_000_000 ≈ 105_350_000.
        // Sum-of-collateral after scale = 105_350_000 / 5 = 21_070_000
        // = exactly the budget. No leg drops below MIN ($10M).
        assert_eq!(gated.len(), 1, "leg is scaled (not skipped) when above MIN");
        let scaled = gated[0].1;
        assert!(
            scaled <= 105_350_001 && scaled >= 105_349_999,
            "scaled-down ETH leg should be ~$105.35, got {scaled}"
        );
        let collateral = scaled / RESIZE_LEVERAGE;
        assert!(
            collateral <= free_usdc_after_reserve,
            "post-scale collateral {collateral} must fit budget {free_usdc_after_reserve}"
        );
        assert!(skips.is_empty(), "no skips when scaling alone fixes it");
    }

    #[test]
    fn pre_flight_usdc_check_prod_shape_when_single_leg_scales_below_min() {
        // Stricter prod-incident variant: same wallet ($21.07) but the
        // leg is small enough that scaling-to-fit would push it under
        // MIN_HEDGE_NOTIONAL_USD ($10M). Required collateral $4M, leg
        // scales to ~$15M notional → $3M collateral. Wait — that's
        // above MIN. Use a budget that forces sub-MIN:
        //   1 leg at $20M notional, required collateral $4M,
        //   budget $1.5M ($1.5M < $4M required). Scaled notional =
        //   $20M * $1.5M / $4M = $7.5M < $10M MIN → leg dropped.
        let legs = vec![("ETH".to_string(), 20_000_000u64)];
        let free_usdc = 1_500_000u64; // $1.50
        let (gated, skips) =
            gate_legs_by_free_usdc(&legs, free_usdc, RESIZE_LEVERAGE, MIN_HEDGE_NOTIONAL_USD);
        assert!(gated.is_empty(), "scaled leg falls below MIN → skipped");
        assert_eq!(skips.len(), 1);
        assert_eq!(skips[0].1, SkipReason::InsufficientUsdcLiquidity);
    }

    #[test]
    fn skip_reason_insufficient_usdc_liquidity_is_distinct() {
        // Pin: the new variant must NOT shadow existing reasons. A
        // future refactor that collapses the enum would break audit
        // log structured-matching for operators.
        assert_ne!(
            SkipReason::InsufficientUsdcLiquidity,
            SkipReason::AlreadyHedged
        );
        assert_ne!(
            SkipReason::InsufficientUsdcLiquidity,
            SkipReason::BelowMinNotional
        );
        assert_ne!(
            SkipReason::InsufficientUsdcLiquidity,
            SkipReason::ZeroExposure
        );
    }
}
