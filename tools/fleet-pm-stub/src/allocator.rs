//! Regime-aware allocator for the Hedgents fleet.
//!
//! A pure decision function that, given the live per-strategy state of
//! the fleet (deployed USD, nominal APR in bps) plus a configurable
//! "risk premium hurdle", emits a single `AllocatorAction`:
//!
//! - `NoAction` — current allocation is already optimal.
//! - `Withdraw(strategy_id, amount_usd, reason)` — a deployed leveraged
//!   strategy's net APR fell below the hurdle over `stable_yield`; unwind.
//! - `Deposit(strategy_id, amount_usd, reason)` — there is idle USDC and
//!   either some leveraged strategy is comfortably above its hurdle, or
//!   none is and we should park in `stable_yield`.
//!
//! The function is intentionally pure and deterministic so that every
//! decision can be replayed and unit-tested. The HTTP plumbing that feeds
//! it lives in `allocator_runner.rs`.
//!
//! ## Decision tree
//!
//! 1. Find `risk_free_apr = stable_yield.nominal_apr_bps`.
//! 2. For each leveraged strategy (everything except `stable_yield`):
//!    `hurdle = risk_free_apr + risk_premium_bps[strategy]`.
//! 3. If any leveraged strategy is deployed AND its APR is **at least
//!    `min_withdraw_gap_bps` below hurdle**, recommend `Withdraw` of its
//!    position (clamped to `max_action_fraction × total_aum_usd`). Pick
//!    the WORST under-hurdle strategy first (largest negative gap).
//!    Step-3 only returns Withdraw when the cap-clamped amount also
//!    clears `min_action_usd`; if it doesn't, we record the observation
//!    and **fall through to step 4** so a small under-hurdle position
//!    cannot block productive deployment of idle capital.
//! 4. If `idle_usd ≥ min_action_usd`:
//!    - if at least one **allocator-deployable** leveraged strategy is
//!      above hurdle, pick the one with the largest positive gap and
//!      `Deposit` idle to it;
//!    - else `Deposit` idle to `stable_yield` (the hurdle anchor — when
//!      noisy spikes push the floor up, that's still the right place
//!      to park).
//!    Strategies that the allocator cannot size in USD (currently
//!    `multiply`, whose `AssignMultiply` envelope has no USD field and
//!    requires an out-of-band wallet transfer) are excluded from the
//!    deposit-target pick — picking them would emit a dead envelope
//!    (`skipped:no_dispatch`) and leave idle un-deployed for the tick.
//! 5. Otherwise `NoAction`.
//!
//! ## Hysteresis: `min_withdraw_gap_bps`
//!
//! Default is 150 bps. The fleet-v0.4.0-rc15 incident (see DEVLOG) was a
//! 352 bps single-tick spike in Kamino's reported USDC supply APR. The
//! orchestrator's 3% risk premium couldn't absorb a 3.5% noise event,
//! and the resulting Withdraw decision liquidated a $174 hedgedjlp
//! position because the envelope layer always sends `u64::MAX` (the
//! daemon's unwind is all-or-nothing today). Until proportional unwind
//! lands in the hedgedjlp daemon, treating Withdraw as "rare and only
//! when carry is truly inverted" is the right behaviour, and a
//! 150-bps-gap hysteresis is the cheapest way to express that.
//!
//! ## Drift-from-target mode (allocator v2 M2)
//!
//! Step 4 of the greedy decision tree above is the *default* picker
//! ("best gap above hurdle wins for Deposit"). Operators who want a
//! *designed* allocation — not the emergent one — set
//! `AllocatorConfig::target_weights` to a [`TargetWeights`] tilt like
//! `(stable=0.30, multiply=0.30, hedgedjlp=0.40)`. The hurdle gate
//! (step 3) still runs identically; only step 4 swaps in `decide_drift_step`:
//!
//! 1. Compute `current_weight - target_weight` per strategy in bps of
//!    `total_aum_usd`.
//! 2. If the most-underweight strategy's drift is inside
//!    `min_drift_bps` (default 200) → `NoAction` "inside rebalance band".
//! 3. Otherwise pick the most-underweight strategy that's also
//!    deployable, above hurdle, and has `target > 0`. If none exist
//!    (e.g. the biggest underweight is under-hurdle), `NoAction`
//!    "no eligible underweight strategy" — but emit the full drift
//!    vector in the audit so the operator can see why.
//! 4. Size the Deposit as `|drift| × total_aum_usd`, capped by
//!    `max_action_fraction × total_aum_usd` and bounded by idle.
//!
//! Overweight strategies are NEVER proactively withdrawn here — that
//! would whipsaw against a strategy whose APR is currently rewarding
//! being there. Overweight corrects itself via (a) the hurdle gate
//! firing if APR drops, or (b) new idle flowing into underweight
//! strategies, lowering the overweight's relative share over time.
//!
//! When `target_weights = None` (the default), the entire drift path
//! is inert and the greedy step-4 logic runs verbatim — every rc15-rc21
//! behavioural test still passes byte-for-byte.

use serde::{Deserialize, Serialize};

use crate::allocator_apr_weighted::{compute_apr_weighted, AprWeightedConfig, GapInput};
use crate::allocator_targets::TargetWeights;

/// How the allocator obtains its per-tick target weights when drift
/// mode is active.
///
/// * `Static` — operator sets a fixed tilt that's used every tick
///   verbatim (allocator v2 M1–M4 behaviour).
/// * `AprWeighted` — weights are recomputed each tick from the live
///   `(APR, hurdle)` per strategy. Higher-yield strategies auto-pull
///   capital toward them, subject to a `stable_yield_floor` baseline
///   and an optional per-strategy minimum (allocator v2 M5).
///
/// `decide()` resolves the mode to a concrete `TargetWeights` at the
/// start of each tick; the rest of the drift-step is mode-agnostic.
#[derive(Debug, Clone, Copy)]
pub enum TargetMode {
    Static(TargetWeights),
    AprWeighted(AprWeightedConfig),
}

impl TargetMode {
    /// Resolve to a concrete `TargetWeights` for the current tick.
    /// `Static` returns the wrapped weights unchanged. `AprWeighted`
    /// computes them from per-strategy gaps.
    pub fn resolve(
        &self,
        strategies: &[StrategyRate],
        cfg: &AllocatorConfig,
        risk_free_bps: i32,
    ) -> TargetWeights {
        match self {
            TargetMode::Static(w) => *w,
            TargetMode::AprWeighted(apr_cfg) => {
                let inputs: Vec<GapInput<'_>> = strategies
                    .iter()
                    .map(|s| {
                        let hurdle = risk_premium_for(&s.id, cfg)
                            .map(|prem| risk_free_bps.saturating_add(prem))
                            .unwrap_or(0); // stable_yield: hurdle = 0 (itself the anchor)
                        GapInput {
                            id: &s.id,
                            apr_bps: s.nominal_apr_bps,
                            hurdle_bps: hurdle,
                        }
                    })
                    .collect();
                compute_apr_weighted(&inputs, apr_cfg)
            }
        }
    }
}

/// Per-strategy live state passed into `decide()`.
#[derive(Debug, Clone, PartialEq)]
pub struct StrategyRate {
    /// Strategy identifier — `"stable_yield"`, `"multiply"`, `"hedgedjlp"`.
    pub id: String,
    /// Current net equity deployed in this strategy, in USD.
    pub deployed_usd: f64,
    /// Current nominal APR (signed bps). `i32` so we can model negative
    /// carry (multiply when borrow > supply) without trickery.
    pub nominal_apr_bps: i32,
}

/// Hurdle + sizing configuration. Values in basis points; 100 bps = 1%.
#[derive(Debug, Clone)]
pub struct AllocatorConfig {
    /// Minimum premium (bps) `multiply` must beat `stable_yield` by to be
    /// worth its risk (liquidation, oracle, smart-contract). Default 200.
    pub risk_premium_bps_multiply: i32,
    /// Minimum premium (bps) `hedgedjlp` must beat `stable_yield` by.
    /// Higher than multiply (default 300) because hedgedjlp carries
    /// funding + JLP basis risk on top of borrow-rate risk.
    pub risk_premium_bps_hedgedjlp: i32,
    /// Skip actions whose USD amount is below this dust threshold.
    pub min_action_usd: f64,
    /// Cap any single action to this fraction of `total_aum_usd`.
    pub max_action_fraction: f64,
    /// Minimum gap (bps) a deployed leveraged strategy must fall below
    /// its hurdle before a `Withdraw` will fire. Default 150 bps — wide
    /// enough that a single noisy APR tick (the fleet-v0.4.0-rc15
    /// incident was a 352 bps spike in stable_yield's reported APR
    /// pushing the hurdle up by the same amount, which would have
    /// liquidated hedgedjlp at gap = -143 bps) cannot trigger an
    /// irreversible all-or-nothing unwind. See module docstring for
    /// the full rationale.
    pub min_withdraw_gap_bps: i32,
    /// Target allocation mode (allocator v2 M2 + M5). When `Some`,
    /// `decide()` runs in drift-from-target mode: after the rc15
    /// hurdle gate, it picks the strategy with the largest absolute
    /// *underweight* drift (current_weight − target_weight) and emits
    /// a Deposit toward it. Overweight strategies are NOT actively
    /// rebalanced — that path would whipsaw against a strategy whose
    /// APR is currently rewarding being there. Operators converge to
    /// target by adding idle capital over time; the orchestrator
    /// chooses where each tranche goes.
    ///
    /// `TargetMode::Static` uses a fixed operator-set tilt (M1).
    /// `TargetMode::AprWeighted` recomputes weights each tick from
    /// per-strategy gap to hurdle (M5).
    ///
    /// When `None` (default), `decide()` runs the rc21 greedy path
    /// (best-gap-above-hurdle wins for Deposit). The two modes share
    /// the same hurdle gate, deployable filter, and sizing caps —
    /// only the deposit-selection step differs.
    pub target_weights: Option<TargetMode>,
    /// Minimum drift (bps, absolute value) from target weight needed
    /// before drift mode emits a Deposit. Default 200 bps. Below this
    /// = "the rebalance band" → NoAction. Without a band the
    /// orchestrator would action on every 1bp wobble — operationally
    /// noisy and inflates the gas bill.
    ///
    /// Only consulted in drift mode (`target_weights = Some(_)`).
    /// Greedy mode ignores it.
    pub min_drift_bps: i32,
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self {
            risk_premium_bps_multiply: 200,
            risk_premium_bps_hedgedjlp: 300,
            min_action_usd: 5.0,
            max_action_fraction: 0.5,
            min_withdraw_gap_bps: 150,
            target_weights: None,
            min_drift_bps: 200,
        }
    }
}

/// The single recommendation emitted per allocator tick. Variants carry
/// the human-readable `reason` string so that audit logs explain *why*
/// the allocator chose this action without re-running the decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AllocatorAction {
    NoAction {
        reason: String,
    },
    Withdraw {
        strategy: String,
        amount_usd: f64,
        reason: String,
    },
    Deposit {
        strategy: String,
        amount_usd: f64,
        reason: String,
    },
}

/// Look up the configured risk premium for a leveraged strategy. Returns
/// `None` for non-leveraged ids (currently just `stable_yield`).
fn risk_premium_for(id: &str, cfg: &AllocatorConfig) -> Option<i32> {
    match id {
        "multiply" => Some(cfg.risk_premium_bps_multiply),
        "hedgedjlp" => Some(cfg.risk_premium_bps_hedgedjlp),
        _ => None,
    }
}

/// Does the allocator know how to emit a USD-sized `Assign` envelope for
/// this strategy? `stable_yield` and `hedgedjlp` both accept a
/// `usdc_lamports` field, but `multiply`'s `AssignMultiply` envelope has
/// no sizing field — the daemon trades against whatever balance sits in
/// its ATA, so allocator-driven deposits would require an out-of-band
/// transfer first.
///
/// The deposit-picker uses this to skip non-deployable strategies, so
/// idle USDC always lands somewhere productive even when `multiply`'s
/// gap is currently the largest.
pub fn is_deployable_via_allocator(id: &str) -> bool {
    !matches!(id, "multiply")
}

/// Format an APR (signed bps) as a `"x.xx%"` string for `reason` strings.
fn fmt_bps(bps: i32) -> String {
    format!("{:.2}%", (bps as f64) / 100.0)
}

/// Per-strategy hurdle classification shared between the greedy and
/// drift dispatcher paths. Negative `gap_bps` = under-hurdle (Withdraw
/// candidate); positive = above-hurdle (Deposit candidate).
struct LevGap<'a> {
    s: &'a StrategyRate,
    hurdle_bps: i32,
    gap_bps: i32, // nominal - hurdle
}

/// Core decision function — pure, deterministic, no I/O. See the module
/// docstring for the decision tree.
pub fn decide(
    strategies: &[StrategyRate],
    total_aum_usd: f64,
    idle_usd: f64,
    cfg: &AllocatorConfig,
) -> AllocatorAction {
    // Find stable_yield as the risk-free reference. If it isn't in the
    // fleet snapshot, refuse to act — we have no hurdle anchor.
    let Some(stable) = strategies.iter().find(|s| s.id == "stable_yield") else {
        return AllocatorAction::NoAction {
            reason: "no stable_yield strategy in snapshot — cannot anchor hurdle".to_string(),
        };
    };
    let risk_free = stable.nominal_apr_bps;

    // Classify leveraged strategies by their gap to hurdle.
    let mut levs: Vec<LevGap> = strategies
        .iter()
        .filter_map(|s| {
            risk_premium_for(&s.id, cfg).map(|prem| {
                let hurdle = risk_free.saturating_add(prem);
                LevGap {
                    s,
                    hurdle_bps: hurdle,
                    gap_bps: s.nominal_apr_bps.saturating_sub(hurdle),
                }
            })
        })
        .collect();

    // Step 3: any DEPLOYED leveraged strategy meaningfully under its
    // hurdle → Withdraw the worst offender (largest negative gap). The
    // hysteresis gate `min_withdraw_gap_bps` is critical: without it, a
    // single-tick spike in `stable_yield`'s reported APR (which Kamino
    // can transiently produce on utilisation flips) is enough to push
    // the hurdle past hedgedjlp's net APR and trigger an irreversible
    // full unwind. The 150-bps default would have survived the rc15
    // incident (actual gap was -143 bps; threshold = 150 → no action).
    //
    // When the worst-under-hurdle position is genuinely below hurdle
    // but the resulting Withdraw amount is below `min_action_usd`, we
    // fall through to step 4 (idle deposit) instead of short-circuiting
    // to NoAction. Otherwise a $8 multiply position sitting 30 bps under
    // hurdle can block $175 of idle USDC from earning anything. We
    // record the observation in `pending_note` so the final reason
    // string still surfaces it if nothing else fires.
    levs.sort_by_key(|l| l.gap_bps); // ascending — worst (most negative) first
    let mut pending_note: Option<String> = None;
    // Clamp to zero so a misconfigured negative threshold doesn't silently
    // turn the gate into "fire Withdraw on any under-hurdle" — that would
    // re-introduce the rc15 incident shape via a config typo. Treat
    // negative as 0 (= "any gap triggers").
    let withdraw_threshold_bps = cfg.min_withdraw_gap_bps.max(0);
    if let Some(worst) = levs.iter().find(|l| {
        l.s.deployed_usd > 0.0 && l.gap_bps < 0 && -l.gap_bps >= withdraw_threshold_bps
    }) {
        let cap = cap_to_aum_fraction(total_aum_usd, cfg.max_action_fraction);
        let amount = worst.s.deployed_usd.min(cap);
        if amount >= cfg.min_action_usd {
            return AllocatorAction::Withdraw {
                strategy: worst.s.id.clone(),
                amount_usd: amount,
                reason: format!(
                    "carry inverted: {} earning {} < hurdle {} ({} + {} risk premium), \
                     gap {} bps clears withdraw threshold {} bps",
                    worst.s.id,
                    fmt_bps(worst.s.nominal_apr_bps),
                    fmt_bps(worst.hurdle_bps),
                    fmt_bps(risk_free),
                    fmt_bps(worst.hurdle_bps - risk_free),
                    worst.gap_bps,
                    withdraw_threshold_bps,
                ),
            };
        }
        pending_note = Some(format!(
            "{} under hurdle ({} < {} = {}+{}), action ${:.2} below min ${:.2} — \
             continuing to idle-deposit check",
            worst.s.id,
            fmt_bps(worst.s.nominal_apr_bps),
            fmt_bps(worst.hurdle_bps),
            fmt_bps(risk_free),
            fmt_bps(worst.hurdle_bps - risk_free),
            amount,
            cfg.min_action_usd,
        ));
    } else if let Some(worst_small) = levs
        .iter()
        .find(|l| l.s.deployed_usd > 0.0 && l.gap_bps < 0)
    {
        // Under-hurdle but inside the hysteresis band — record but don't
        // act. This is the noise-absorbing case.
        pending_note = Some(format!(
            "{} under hurdle ({} < {} = {}+{}) but gap {} bps inside hysteresis band \
             (threshold {} bps) — treating as noise",
            worst_small.s.id,
            fmt_bps(worst_small.s.nominal_apr_bps),
            fmt_bps(worst_small.hurdle_bps),
            fmt_bps(risk_free),
            fmt_bps(worst_small.hurdle_bps - risk_free),
            worst_small.gap_bps,
            withdraw_threshold_bps,
        ));
    }

    // Dispatch to the deposit-selection step. Both modes share the
    // hurdle gate above; only the picker differs.
    //
    // Greedy mode (rc21 default): largest-positive-gap-above-hurdle wins
    // for Deposit; stable_yield is the safe fallback when nothing beats
    // hurdle. Drift mode (M2 opt-in via target_weights): largest
    // absolute *underweight* drift wins; the rebalance band
    // (min_drift_bps) absorbs small wobbles.
    //
    // M5: in drift mode, the TargetMode is resolved to a concrete
    // TargetWeights ONCE here. `Static` returns the fixed tilt verbatim;
    // `AprWeighted` computes weights from the current snapshot. Either
    // way, decide_drift_step downstream is mode-agnostic.
    match cfg.target_weights {
        Some(ref mode) => {
            let resolved = mode.resolve(strategies, cfg, risk_free);
            decide_drift_step(
                strategies,
                total_aum_usd,
                idle_usd,
                cfg,
                &resolved,
                &levs,
                pending_note,
            )
        }
        None => decide_greedy_step(
            total_aum_usd,
            idle_usd,
            cfg,
            risk_free,
            &mut levs,
            pending_note,
        ),
    }
}

/// rc21 greedy deposit selection — kept as the default to preserve
/// every behavioural test from the rc15-rc21 arc. Best deployable
/// leveraged strategy above hurdle wins; stable_yield is the safe
/// fallback. Behaviourally identical to the pre-M2 step-4/step-5 body.
fn decide_greedy_step(
    total_aum_usd: f64,
    idle_usd: f64,
    cfg: &AllocatorConfig,
    risk_free: i32,
    levs: &mut [LevGap<'_>],
    pending_note: Option<String>,
) -> AllocatorAction {
    // Step 4: idle cash present and above min-action.
    if idle_usd >= cfg.min_action_usd {
        let cap = cap_to_aum_fraction(total_aum_usd, cfg.max_action_fraction);
        let amount = idle_usd.min(cap);
        if amount >= cfg.min_action_usd {
            // Best DEPLOYABLE leveraged strategy = largest positive gap
            // to hurdle, excluding strategies the allocator cannot size
            // in USD.
            levs.sort_by_key(|l| -l.gap_bps); // descending — best first
            let best_deployable = levs
                .iter()
                .find(|l| l.gap_bps > 0 && is_deployable_via_allocator(&l.s.id));
            if let Some(best) = best_deployable {
                let mut reason = format!(
                    "{} beats hurdle by {} ({} vs {} = {}+{} hurdle)",
                    best.s.id,
                    fmt_bps(best.gap_bps),
                    fmt_bps(best.s.nominal_apr_bps),
                    fmt_bps(best.hurdle_bps),
                    fmt_bps(risk_free),
                    fmt_bps(best.hurdle_bps - risk_free),
                );
                if let Some(note) = &pending_note {
                    reason = format!("{} (also: {})", reason, note);
                }
                return AllocatorAction::Deposit {
                    strategy: best.s.id.clone(),
                    amount_usd: amount,
                    reason,
                };
            }
            // No deployable leveraged above hurdle → park in stable_yield.
            let mut reason = format!(
                "no deployable leveraged above hurdle; park idle ${:.2} in stable_yield @ {}",
                idle_usd,
                fmt_bps(risk_free),
            );
            if let Some(note) = &pending_note {
                reason = format!("{} (also: {})", reason, note);
            }
            return AllocatorAction::Deposit {
                strategy: "stable_yield".to_string(),
                amount_usd: amount,
                reason,
            };
        }
        let reason = match pending_note {
            Some(note) => format!(
                "idle ${:.2} caps to ${:.2} (below min ${:.2}); {}",
                idle_usd, amount, cfg.min_action_usd, note
            ),
            None => format!(
                "idle ${:.2} present but max_action_fraction caps action to ${:.2} \
                 (below min ${:.2})",
                idle_usd, amount, cfg.min_action_usd,
            ),
        };
        return AllocatorAction::NoAction { reason };
    }

    let reason = match pending_note {
        Some(note) => format!(
            "{}; idle ${:.2} below min ${:.2}",
            note, idle_usd, cfg.min_action_usd
        ),
        None => format!(
            "all deployed strategies meet hurdle; idle ${:.2} below min ${:.2}",
            idle_usd, cfg.min_action_usd,
        ),
    };
    AllocatorAction::NoAction { reason }
}

/// Drift-from-target deposit selection (allocator v2 M2).
///
/// For each strategy, compute `current_weight - target_weight` as bps
/// of `total_aum_usd`. Pick the strategy with the largest *underweight*
/// drift (most negative `drift_bps`) among eligible candidates:
///   - must be allocator-deployable in USD (`multiply` excluded — its
///     Assign envelope has no USD field; same gate as greedy mode)
///   - must NOT be under hurdle (don't reinforce a broken strategy)
///   - must have target_weight > 0 (no point depositing into a
///     strategy the operator explicitly wants empty)
///
/// If the largest underweight is inside the rebalance band
/// (`|drift| < cfg.min_drift_bps`) → NoAction. This absorbs the wobble
/// that single-tick APR noise creates and keeps the gas bill bounded.
///
/// Overweight strategies are NEVER actively withdrawn here — that path
/// would whipsaw against a strategy whose high APR is rewarding being
/// there. Overweight gets corrected naturally by: (a) hurdle gate
/// withdrawing if APR drops, or (b) new idle flowing into underweight
/// strategies, lowering the overweight's *relative* share over time.
fn decide_drift_step(
    strategies: &[StrategyRate],
    total_aum_usd: f64,
    idle_usd: f64,
    cfg: &AllocatorConfig,
    targets: &TargetWeights,
    levs: &[LevGap<'_>],
    pending_note: Option<String>,
) -> AllocatorAction {
    let drift_band = cfg.min_drift_bps.max(0);

    if total_aum_usd <= 0.0 {
        // Pathological snapshot. The rest of decide_drift_step assumes
        // total_aum_usd > 0 for the drift math; fall back to a NoAction
        // rather than dividing by zero.
        return AllocatorAction::NoAction {
            reason: "drift mode: total_aum_usd <= 0; cannot compute weights".to_string(),
        };
    }

    // Compute drift per strategy. drift_bps > 0 = overweight.
    // We retain the drifts for the audit-reason string regardless of
    // whether a strategy is eligible to receive a Deposit — the
    // operator wants to see the whole vector.
    let mut rows: Vec<DriftRow> = strategies
        .iter()
        .map(|s| {
            let target = targets.for_strategy(&s.id);
            let current = s.deployed_usd / total_aum_usd;
            let drift = current - target;
            let drift_bps = (drift * 10_000.0).round() as i32;
            // Eligibility: deployable, target > 0, and (stable_yield OR
            // above hurdle). stable_yield has no hurdle (it IS the
            // hurdle anchor), so it's always above; leveraged strategies
            // must clear their per-strategy hurdle before receiving a
            // Deposit.
            let above_hurdle = if s.id == "stable_yield" {
                true
            } else {
                levs.iter()
                    .find(|l| l.s.id == s.id)
                    .map(|l| l.gap_bps > 0)
                    .unwrap_or(false)
            };
            let eligible =
                is_deployable_via_allocator(&s.id) && target > 0.0 && above_hurdle;
            DriftRow {
                id: s.id.clone(),
                current_weight: current,
                target_weight: target,
                drift_bps,
                eligible,
            }
        })
        .collect();

    // Sort by drift_bps ascending so the most-underweight strategies
    // come first (largest negative drift = furthest below target).
    rows.sort_by_key(|r| r.drift_bps);

    // Band check FIRST, before eligibility — the operational semantics
    // are:
    //   - "everyone at target" and "everyone within wobble of target"
    //     are the same outcome (nothing to do).
    //   - "max underweight is in band BUT it's an ineligible strategy"
    //     is also the same outcome — the noise isn't actionable anyway.
    //
    // Without this ordering a strategy that's only 50 bps underweight
    // but happens to be ineligible would hit the "no eligible" branch
    // instead of the "inside band" branch, which is technically
    // correct but operationally misleading for the audit log.
    let most_underweight_bps = rows
        .first()
        .map(|r| (-r.drift_bps).max(0))
        .unwrap_or(0);
    if most_underweight_bps < drift_band {
        return no_action_with_drift_summary(
            &rows,
            idle_usd,
            cfg.min_action_usd,
            &format!(
                "drift mode: largest underweight {} bps inside rebalance band ({} bps)",
                most_underweight_bps, drift_band
            ),
            pending_note,
        );
    }

    // Past the band — find the most-underweight ELIGIBLE candidate.
    // `eligible` already encodes "deployable AND above hurdle AND
    // target > 0", so this naturally falls back from
    // most-underweight-overall to most-underweight-actionable.
    let best = rows.iter().find(|r| r.eligible && r.drift_bps < 0);

    let Some(best) = best else {
        // Big drift exists but no eligible target — either the
        // most-underweight strategy is non-deployable / under-hurdle /
        // has target=0 and all others are at or above target. Surface
        // the full drift vector so the operator can see why.
        return no_action_with_drift_summary(
            &rows,
            idle_usd,
            cfg.min_action_usd,
            "drift mode: no eligible underweight strategy",
            pending_note,
        );
    };

    let underweight_bps = -best.drift_bps; // positive = how far below target

    // Sizing: dollar amount needed to fully close the drift, capped by
    // max_action_fraction × total_aum and bounded by the idle pool.
    let drift_dollars = (underweight_bps as f64 / 10_000.0) * total_aum_usd;
    let cap = cap_to_aum_fraction(total_aum_usd, cfg.max_action_fraction);
    let amount = drift_dollars.min(cap).min(idle_usd);

    if amount < cfg.min_action_usd {
        return no_action_with_drift_summary(
            &rows,
            idle_usd,
            cfg.min_action_usd,
            &format!(
                "drift mode: {} underweight by {} bps but action ${:.2} below min ${:.2}",
                best.id, underweight_bps, amount, cfg.min_action_usd
            ),
            pending_note,
        );
    }

    let mut reason = format!(
        "drift mode: {} underweight by {} bps (current {:.1}% vs target {:.1}%); \
         depositing ${:.2} to close",
        best.id,
        underweight_bps,
        best.current_weight * 100.0,
        best.target_weight * 100.0,
        amount,
    );
    if let Some(note) = &pending_note {
        reason = format!("{} (also: {})", reason, note);
    }

    AllocatorAction::Deposit {
        strategy: best.id.clone(),
        amount_usd: amount,
        reason,
    }
}

/// Drift snapshot for one strategy. Built per-tick by `decide_drift_step`.
/// Kept private to the module — drift-mode internals are not part of
/// the public API yet; only the resulting `AllocatorAction` is.
#[derive(Debug)]
struct DriftRow {
    id: String,
    current_weight: f64,
    target_weight: f64,
    /// (current - target) × 10_000. Positive = overweight, negative = underweight.
    drift_bps: i32,
    /// Allocator-deployable AND above-hurdle AND target > 0.
    eligible: bool,
}

/// Common no-action exit for the drift-step's "didn't fire" branches.
/// Always includes a compact summary of the drift vector so the audit
/// log captures the full state, not just the picked strategy.
fn no_action_with_drift_summary(
    rows: &[DriftRow],
    idle_usd: f64,
    min_action_usd: f64,
    headline: &str,
    pending_note: Option<String>,
) -> AllocatorAction {
    let summary = rows
        .iter()
        .map(|r| format!("{}={:+}bps", r.id, r.drift_bps))
        .collect::<Vec<_>>()
        .join(",");
    let mut reason = format!(
        "{} [drifts: {}]; idle ${:.2}, min ${:.2}",
        headline, summary, idle_usd, min_action_usd
    );
    if let Some(note) = &pending_note {
        reason = format!("{} (also: {})", reason, note);
    }
    AllocatorAction::NoAction { reason }
}

/// Cap a candidate USD amount by the configured AUM fraction. Returns
/// `f64::INFINITY` if AUM is zero or fraction is non-positive (no cap).
fn cap_to_aum_fraction(total_aum_usd: f64, fraction: f64) -> f64 {
    if total_aum_usd <= 0.0 || fraction <= 0.0 {
        f64::INFINITY
    } else {
        total_aum_usd * fraction
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sr(id: &str, deployed: f64, apr_bps: i32) -> StrategyRate {
        StrategyRate {
            id: id.to_string(),
            deployed_usd: deployed,
            nominal_apr_bps: apr_bps,
        }
    }

    fn cfg() -> AllocatorConfig {
        AllocatorConfig::default()
    }

    #[test]
    fn no_stable_yield_anchors_no_action() {
        let s = vec![sr("multiply", 100.0, 1500)];
        match decide(&s, 100.0, 0.0, &cfg()) {
            AllocatorAction::NoAction { reason } => assert!(reason.contains("stable_yield")),
            other => panic!("expected NoAction, got {:?}", other),
        }
    }

    #[test]
    fn only_stable_idle_zero_no_action() {
        let s = vec![
            sr("stable_yield", 1000.0, 701),
            sr("multiply", 0.0, 1500),
            sr("hedgedjlp", 0.0, 2000),
        ];
        match decide(&s, 1000.0, 0.0, &cfg()) {
            AllocatorAction::NoAction { .. } => {}
            other => panic!("expected NoAction, got {:?}", other),
        }
    }

    #[test]
    fn multiply_far_below_hurdle_withdraws() {
        // stable 7.01% + 2% = 9.01% hurdle. Multiply at 5% → withdraw.
        let s = vec![
            sr("stable_yield", 500.0, 701),
            sr("multiply", 500.0, 500),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 1000.0, 0.0, &cfg()) {
            AllocatorAction::Withdraw {
                strategy,
                amount_usd,
                reason,
            } => {
                assert_eq!(strategy, "multiply");
                // capped by max_action_fraction = 0.5 * 1000 = 500, equal
                // to deployed → withdraw 500 exactly.
                assert!((amount_usd - 500.0).abs() < 1e-9);
                assert!(reason.contains("carry inverted"));
            }
            other => panic!("expected Withdraw, got {:?}", other),
        }
    }

    #[test]
    fn multiply_just_below_hurdle_inside_hysteresis_is_noise() {
        // stable 7% + 2% = 9% hurdle. Multiply at 8.99% → gap = -1 bps.
        // With default `min_withdraw_gap_bps = 150`, this is well inside
        // the noise band and must NOT trigger a Withdraw. This is the
        // fleet-v0.4.0-rc15 regression: a single-tick APR jitter is not
        // a real carry-inversion signal.
        let s = vec![
            sr("stable_yield", 500.0, 700),
            sr("multiply", 100.0, 899),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 600.0, 0.0, &cfg()) {
            AllocatorAction::NoAction { reason } => {
                assert!(
                    reason.contains("hysteresis"),
                    "expected hysteresis-band reason, got: {reason}"
                );
            }
            other => panic!("expected NoAction (inside hysteresis band), got {other:?}"),
        }
    }

    #[test]
    fn multiply_just_below_hurdle_with_idle_still_deploys_idle() {
        // Same shape as above but with idle USDC present. The fleet
        // must not let a tiny under-hurdle blocker stop the idle from
        // getting deployed. Idle parks in stable_yield (deployable),
        // and the audit reason still surfaces the under-hurdle note.
        let s = vec![
            sr("stable_yield", 500.0, 700),
            sr("multiply", 100.0, 899), // gap -1, inside band
            sr("hedgedjlp", 0.0, 800),  // below 7+3=10% hurdle, no deposit candidate
        ];
        match decide(&s, 700.0, 100.0, &cfg()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                reason,
            } => {
                assert_eq!(strategy, "stable_yield");
                assert!((amount_usd - 100.0).abs() < 1e-9);
                assert!(
                    reason.contains("hysteresis"),
                    "audit reason should preserve the under-hurdle observation: {reason}"
                );
            }
            other => panic!("expected Deposit to stable_yield, got {other:?}"),
        }
    }

    #[test]
    fn multiply_above_hurdle_no_idle_no_action() {
        let s = vec![
            sr("stable_yield", 500.0, 700),
            sr("multiply", 500.0, 1200),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 1000.0, 0.0, &cfg()) {
            AllocatorAction::NoAction { .. } => {}
            other => panic!("expected NoAction, got {:?}", other),
        }
    }

    #[test]
    fn multiply_above_hurdle_with_idle_falls_through_to_stable_yield() {
        // multiply is above hurdle (gap=+300), but the allocator cannot
        // size an AssignMultiply envelope in USD — `is_deployable_via_allocator`
        // returns false for "multiply". The picker must skip it and
        // pick the next deployable above-hurdle strategy, falling back
        // to stable_yield when none qualifies. Without this filter the
        // orchestrator would emit a dead Deposit→multiply envelope
        // (`skipped:no_dispatch`) and the idle USDC would never land.
        let s = vec![
            sr("stable_yield", 500.0, 700),
            sr("multiply", 500.0, 1200),
            sr("hedgedjlp", 0.0, 800), // below 7+3 = 10% hurdle
        ];
        match decide(&s, 1050.0, 50.0, &cfg()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                ..
            } => {
                assert_eq!(strategy, "stable_yield");
                assert!((amount_usd - 50.0).abs() < 1e-9);
            }
            other => panic!("expected Deposit to stable_yield, got {other:?}"),
        }
    }

    #[test]
    fn deployable_filter_picks_hedgedjlp_over_higher_gap_multiply() {
        // multiply gap = 1500-900 = +600 (largest), hedgedjlp gap =
        // 1300-1000 = +300. Without the deployable filter the picker
        // would pick multiply and the envelope layer would return None.
        // With the filter, hedgedjlp wins.
        let s = vec![
            sr("stable_yield", 100.0, 700),
            sr("multiply", 100.0, 1500),
            sr("hedgedjlp", 100.0, 1300),
        ];
        match decide(&s, 300.0, 50.0, &cfg()) {
            AllocatorAction::Deposit { strategy, .. } => assert_eq!(strategy, "hedgedjlp"),
            other => panic!("expected Deposit(hedgedjlp), got {other:?}"),
        }
    }

    #[test]
    fn idle_below_min_action_no_action() {
        let s = vec![
            sr("stable_yield", 100.0, 700),
            sr("multiply", 0.0, 1500),
            sr("hedgedjlp", 0.0, 2000),
        ];
        match decide(&s, 100.0, 2.0, &cfg()) {
            AllocatorAction::NoAction { .. } => {}
            other => panic!("expected NoAction, got {:?}", other),
        }
    }

    #[test]
    fn withdraw_clamped_by_max_action_fraction() {
        // deployed $1000 in multiply, total $1500, fraction 0.5 → cap $750.
        let s = vec![
            sr("stable_yield", 500.0, 700),
            sr("multiply", 1000.0, 200), // way below hurdle
            sr("hedgedjlp", 0.0, 800),
        ];
        let c = AllocatorConfig {
            max_action_fraction: 0.5,
            ..AllocatorConfig::default()
        };
        match decide(&s, 1500.0, 0.0, &c) {
            AllocatorAction::Withdraw { amount_usd, .. } => {
                assert!(
                    (amount_usd - 750.0).abs() < 1e-9,
                    "expected 750, got {amount_usd}"
                );
            }
            other => panic!("expected Withdraw, got {:?}", other),
        }
    }

    #[test]
    fn no_leveraged_above_hurdle_idle_parks_in_stable() {
        let s = vec![
            sr("stable_yield", 100.0, 700),
            sr("multiply", 0.0, 800),  // below 7+2 = 9
            sr("hedgedjlp", 0.0, 900), // below 7+3 = 10
        ];
        match decide(&s, 100.0, 50.0, &cfg()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                ..
            } => {
                assert_eq!(strategy, "stable_yield");
                assert!((amount_usd - 50.0).abs() < 1e-9);
            }
            other => panic!("expected Deposit to stable_yield, got {:?}", other),
        }
    }

    #[test]
    fn worst_under_hurdle_picked_first() {
        // Both leveraged below hurdle; multiply gap = -500 bps, hedgedjlp = -100.
        // Worst (multiply) should be selected.
        let s = vec![
            sr("stable_yield", 500.0, 700),
            sr("multiply", 200.0, 400),  // gap = 400 - 900 = -500
            sr("hedgedjlp", 200.0, 900), // gap = 900 - 1000 = -100
        ];
        match decide(&s, 900.0, 0.0, &cfg()) {
            AllocatorAction::Withdraw { strategy, .. } => assert_eq!(strategy, "multiply"),
            other => panic!("expected Withdraw(multiply), got {:?}", other),
        }
    }

    #[test]
    fn best_above_hurdle_picked_for_deposit() {
        // multiply gap = 1200 - 900 = +300, hedgedjlp = 1500 - 1000 = +500.
        // hedgedjlp wins.
        let s = vec![
            sr("stable_yield", 100.0, 700),
            sr("multiply", 100.0, 1200),
            sr("hedgedjlp", 100.0, 1500),
        ];
        match decide(&s, 300.0, 100.0, &cfg()) {
            AllocatorAction::Deposit { strategy, .. } => assert_eq!(strategy, "hedgedjlp"),
            other => panic!("expected Deposit(hedgedjlp), got {:?}", other),
        }
    }

    #[test]
    fn withdraw_amount_below_min_action_falls_through_to_no_action() {
        // Deployed only $2, below min_action $5, no idle → NoAction
        // (step 4 also doesn't fire since idle is 0). The audit reason
        // still surfaces the under-hurdle observation.
        let s = vec![sr("stable_yield", 100.0, 700), sr("multiply", 2.0, 100)];
        let c = AllocatorConfig {
            max_action_fraction: 1.0,
            ..AllocatorConfig::default()
        };
        match decide(&s, 102.0, 0.0, &c) {
            AllocatorAction::NoAction { reason } => {
                assert!(reason.contains("below min"), "got: {reason}");
            }
            other => panic!("expected NoAction, got {other:?}"),
        }
    }

    #[test]
    fn rc15_regression_apr_spike_does_not_trigger_full_unwind() {
        // The exact incident shape from fleet-v0.4.0-rc15:
        //   stable_yield 8.96% (post-spike, was 5.44% one tick earlier)
        //   multiply 11.61%
        //   hedgedjlp 10.53%, $173.82 deployed
        // Hurdles: multiply 10.96%, hedgedjlp 11.96%.
        // hedgedjlp gap = 1053 - 1196 = -143 bps.
        // With default min_withdraw_gap_bps = 150, |gap| 143 < 150 →
        // inside hysteresis band → MUST NOT trigger Withdraw.
        let s = vec![
            sr("stable_yield", 55.20, 896),
            sr("multiply", 8.33, 1161),
            sr("hedgedjlp", 173.82, 1053),
        ];
        match decide(&s, 239.33, 1.98, &cfg()) {
            AllocatorAction::Withdraw { .. } => {
                panic!("rc15 regression: APR-spike must not trigger Withdraw")
            }
            AllocatorAction::Deposit { .. } | AllocatorAction::NoAction { .. } => {}
        }
    }

    #[test]
    fn rc15_regression_post_unwind_idle_redeploys() {
        // Post-incident state: $175 idle, multiply $8.33 slightly under
        // its $10.96% hurdle (gap -35 bps, inside hysteresis), hedgedjlp
        // $0. Before fleet-v0.4.0-rc15, this state returned NoAction for
        // ~5 hours because the under-hurdle multiply blocked the
        // idle-deposit path. The fix is to fall through to step 4 and
        // deploy the idle into stable_yield (the only deployable
        // strategy not above its hurdle here).
        let s = vec![
            sr("stable_yield", 55.20, 896),
            sr("multiply", 8.33, 1061), // gap -35, inside band
            sr("hedgedjlp", 0.0, 1040), // not deployed, below hurdle anyway
        ];
        match decide(&s, 239.37, 175.84, &cfg()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                ..
            } => {
                assert_eq!(strategy, "stable_yield");
                // idle 175.84 capped by max_action_fraction 0.5 * 239.37 = 119.685
                assert!(
                    (amount_usd - 119.685).abs() < 1e-9,
                    "expected 119.685, got {amount_usd}"
                );
            }
            other => panic!("expected Deposit to stable_yield, got {other:?}"),
        }
    }

    #[test]
    fn hysteresis_threshold_exactly_triggers_withdraw() {
        // gap = -150 bps exactly = threshold. Triggers Withdraw
        // (inclusive boundary).
        let s = vec![
            sr("stable_yield", 100.0, 700),
            sr("multiply", 100.0, 750), // gap = 750 - 900 = -150
        ];
        match decide(&s, 200.0, 0.0, &cfg()) {
            AllocatorAction::Withdraw { strategy, .. } => assert_eq!(strategy, "multiply"),
            other => panic!("expected Withdraw at threshold boundary, got {other:?}"),
        }
    }

    #[test]
    fn hysteresis_one_below_threshold_no_withdraw() {
        // gap = -149 bps, one bp inside the band → no Withdraw.
        let s = vec![
            sr("stable_yield", 100.0, 700),
            sr("multiply", 100.0, 751), // gap = 751 - 900 = -149
        ];
        match decide(&s, 200.0, 0.0, &cfg()) {
            AllocatorAction::NoAction { reason } => {
                assert!(reason.contains("hysteresis"), "got: {reason}");
            }
            other => panic!("expected NoAction inside hysteresis band, got {other:?}"),
        }
    }

    #[test]
    fn is_deployable_via_allocator_filter() {
        assert!(is_deployable_via_allocator("stable_yield"));
        assert!(is_deployable_via_allocator("hedgedjlp"));
        assert!(!is_deployable_via_allocator("multiply"));
        // Unknown strategies default to deployable — the envelope-spec
        // layer is the second gate that catches truly-unknown ids.
        assert!(is_deployable_via_allocator("some_future_strategy"));
    }

    #[test]
    fn negative_min_withdraw_gap_bps_clamped_to_zero() {
        // Audit follow-up: a misconfigured negative threshold (operator
        // typo) must not silently turn the gate into "fire withdraw on
        // any under-hurdle" — that would re-introduce the rc15 incident
        // shape. The implementation clamps to 0 at use-site, which means
        // "any gap triggers" (the pre-rc15 behavior), not "any value
        // triggers." Combined with the existing `gap_bps < 0` guard, a
        // negative threshold is degenerate-but-safe.
        let s = vec![
            sr("stable_yield", 500.0, 700),
            sr("multiply", 100.0, 899), // gap = -1 bp
        ];
        let c = AllocatorConfig {
            min_withdraw_gap_bps: -150,
            ..AllocatorConfig::default()
        };
        // With clamp-to-0, the -1 bp gap clears threshold 0 → Withdraw.
        // Amount = $100 capped at 0.5 * 600 = $300, so $100 actual.
        match decide(&s, 600.0, 0.0, &c) {
            AllocatorAction::Withdraw { strategy, .. } => {
                assert_eq!(strategy, "multiply");
            }
            other => panic!(
                "negative threshold should clamp to 0 (not amplify) — expected \
                 Withdraw, got {other:?}"
            ),
        }
    }

    #[test]
    fn default_config_derived_invariants() {
        // rc21 audit M2: pre-rc21 the default values were never directly
        // pinned by tests — behavioural tests using `cfg()` would
        // silently re-calibrate against any decimal typo (200 → 20)
        // because the assertions were already computed off the typo'd
        // value. Pin the *invariants* the docstrings imply: hedgedjlp's
        // risk premium must exceed multiply's (the docstring at line
        // 84-86 explicitly justifies the asymmetry), the withdraw gap
        // must be larger than the rc15 incident gap so noise of that
        // shape can't trigger a liquidation, and the floors/ceilings
        // catch order-of-magnitude typos.
        let cfg = AllocatorConfig::default();

        // hedgedjlp carries funding + JLP basis risk on top of borrow
        // rate risk; its risk premium MUST exceed multiply's. Flipping
        // this would mean the allocator considered multiply riskier
        // than the perp-hedge basis, which contradicts the strategy
        // taxonomy.
        assert!(
            cfg.risk_premium_bps_hedgedjlp > cfg.risk_premium_bps_multiply,
            "hedgedjlp risk premium ({}) must exceed multiply ({}) per \
             docstring rationale",
            cfg.risk_premium_bps_hedgedjlp,
            cfg.risk_premium_bps_multiply
        );

        // Sanity ranges (catches decimal typos). Both risk premiums
        // should be 100–500 bps. Outside that window, either we've
        // shifted strategy risk dramatically (which should land in
        // multiple test updates) or someone fat-fingered a zero.
        assert!(
            (100..=500).contains(&cfg.risk_premium_bps_multiply),
            "risk_premium_bps_multiply ({}) outside 100-500bps sanity \
             window — likely a typo",
            cfg.risk_premium_bps_multiply
        );
        assert!(
            (100..=500).contains(&cfg.risk_premium_bps_hedgedjlp),
            "risk_premium_bps_hedgedjlp ({}) outside 100-500bps sanity \
             window — likely a typo",
            cfg.risk_premium_bps_hedgedjlp
        );

        // Withdraw gap must be strictly greater than the fleet-v0.4.0-rc15
        // incident gap (-143 bps) so a re-run of that shape no longer
        // trips the gate. This is the *load-bearing* invariant: any
        // future reduction below 143 silently re-introduces the rc15
        // incident.
        assert!(
            cfg.min_withdraw_gap_bps > 143,
            "min_withdraw_gap_bps ({}) must exceed the rc15 incident gap \
             of 143bps — see DEVLOG rc15 entry",
            cfg.min_withdraw_gap_bps
        );
        // Sanity ceiling: ≥ 1000 bps would mean withdraw never fires
        // in normal operating conditions, which defeats the purpose of
        // an allocator.
        assert!(
            cfg.min_withdraw_gap_bps < 1_000,
            "min_withdraw_gap_bps ({}) ≥ 1000bps would never trigger — \
             allocator becomes ornamental",
            cfg.min_withdraw_gap_bps
        );

        // Sizing knobs: action threshold + fraction must be sane.
        assert!(cfg.min_action_usd > 0.0);
        assert!(cfg.min_action_usd < 1_000.0);
        assert!(cfg.max_action_fraction > 0.0);
        assert!(cfg.max_action_fraction <= 1.0);
    }

    // ── Allocator v2 (M2) — drift-from-target mode tests ────────────────

    fn cfg_with_targets(s: f64, m: f64, h: f64) -> AllocatorConfig {
        AllocatorConfig {
            target_weights: Some(TargetMode::Static(
                TargetWeights::new(s, m, h).expect("test weights"),
            )),
            // Loosen the action floor so $5 underweight at modest AUM still
            // clears the gate in unit tests.
            min_action_usd: 1.0,
            // Default rebalance band; explicit so tests document the value
            // they exercise rather than inheriting silently.
            min_drift_bps: 200,
            ..AllocatorConfig::default()
        }
    }

    #[test]
    fn drift_mode_equal_targets_equal_current_no_action() {
        // Targets: 0.30/0.30/0.40. Current: stable=$30, multiply=$30,
        // hedgedjlp=$40, idle=$0 (sums to 1.0 of $100 AUM). Every drift
        // is 0 → inside band → NoAction.
        let s = vec![
            sr("stable_yield", 30.0, 500),
            sr("multiply", 30.0, 1500),
            sr("hedgedjlp", 40.0, 1500),
        ];
        match decide(&s, 100.0, 0.0, &cfg_with_targets(0.30, 0.30, 0.40)) {
            AllocatorAction::NoAction { reason } => {
                assert!(
                    reason.contains("rebalance band") || reason.contains("inside"),
                    "expected band-related reason, got: {reason}"
                );
            }
            other => panic!("expected NoAction at on-target, got {other:?}"),
        }
    }

    #[test]
    fn drift_mode_tilted_target_with_idle_deposits_largest_underweight() {
        // Target 0.30/0.30/0.40. Current: stable=$0, multiply=$0,
        // hedgedjlp=$0, idle=$100. All three are 100% underweight
        // (current 0%, target up to 40%). hedgedjlp has the largest
        // *absolute* underweight (-4000 bps) → wins.
        let s = vec![
            sr("stable_yield", 0.0, 500),
            sr("multiply", 0.0, 1500),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 100.0, 100.0, &cfg_with_targets(0.30, 0.30, 0.40)) {
            AllocatorAction::Deposit {
                strategy, reason, ..
            } => {
                assert_eq!(strategy, "hedgedjlp", "biggest underweight should win");
                assert!(reason.contains("drift mode"), "audit should label mode: {reason}");
                assert!(
                    reason.contains("underweight"),
                    "audit should name direction: {reason}"
                );
            }
            other => panic!("expected Deposit to hedgedjlp, got {other:?}"),
        }
    }

    #[test]
    fn drift_mode_multiply_excluded_from_picker_even_when_most_underweight() {
        // Target 0.10/0.50/0.40 — multiply is the biggest target, so
        // it would be the most-underweight pick if eligible. But
        // is_deployable_via_allocator("multiply") = false (AssignMultiply
        // has no USD field), so the picker must skip it and choose
        // the next-most-underweight DEPLOYABLE candidate (hedgedjlp).
        let s = vec![
            sr("stable_yield", 0.0, 500),
            sr("multiply", 0.0, 1500),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 100.0, 100.0, &cfg_with_targets(0.10, 0.50, 0.40)) {
            AllocatorAction::Deposit { strategy, .. } => {
                assert_eq!(strategy, "hedgedjlp", "multiply excluded → hedgedjlp wins");
            }
            other => panic!("expected Deposit(hedgedjlp), got {other:?}"),
        }
    }

    #[test]
    fn drift_mode_under_hurdle_strategy_not_picked_for_deposit() {
        // hedgedjlp is below hurdle (APR 800 vs hurdle 500+300=800 → gap
        // is exactly 0, not above). Even though it's most underweight
        // (target 0.40, current 0.0), drift mode must skip it.
        // stable_yield wins instead.
        let s = vec![
            sr("stable_yield", 0.0, 500),
            sr("multiply", 0.0, 1500), // above hurdle but not deployable
            sr("hedgedjlp", 0.0, 800), // gap = 800 - 800 = 0 → NOT above hurdle
        ];
        match decide(&s, 100.0, 100.0, &cfg_with_targets(0.30, 0.30, 0.40)) {
            AllocatorAction::Deposit { strategy, .. } => {
                assert_eq!(
                    strategy, "stable_yield",
                    "under-hurdle hedgedjlp must be skipped → stable_yield wins"
                );
            }
            other => panic!("expected Deposit(stable_yield), got {other:?}"),
        }
    }

    #[test]
    fn drift_mode_hurdle_gate_takes_precedence_over_drift_selection() {
        // multiply is far under hurdle ($100 deployed, APR 100 vs hurdle
        // 700+200=900 → gap = -800 bps, well past the 150-bps default
        // threshold). hedgedjlp is severely underweight. In drift mode
        // the *hurdle gate* still fires first → Withdraw multiply, not
        // Deposit hedgedjlp.
        let s = vec![
            sr("stable_yield", 30.0, 700),
            sr("multiply", 100.0, 100), // far under hurdle
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 130.0, 0.0, &cfg_with_targets(0.30, 0.30, 0.40)) {
            AllocatorAction::Withdraw { strategy, .. } => {
                assert_eq!(strategy, "multiply", "hurdle gate must precede drift");
            }
            other => panic!("expected Withdraw(multiply), got {other:?}"),
        }
    }

    #[test]
    fn drift_mode_no_whipsaw_on_overweight_above_hurdle() {
        // hedgedjlp is overweight (current 0.50, target 0.40) AND above
        // hurdle. Drift mode must NOT withdraw — the high APR is
        // rewarding being there; overweight gets corrected by future
        // inflows to underweight strategies, not by proactive withdraw.
        let s = vec![
            sr("stable_yield", 30.0, 500),
            sr("multiply", 20.0, 1500),
            sr("hedgedjlp", 50.0, 1500),
        ];
        // No idle → can't deposit. Test expects NoAction (NOT Withdraw).
        match decide(&s, 100.0, 0.0, &cfg_with_targets(0.30, 0.30, 0.40)) {
            AllocatorAction::Withdraw { .. } => {
                panic!("drift mode must not actively withdraw an overweight-but-healthy strategy")
            }
            AllocatorAction::NoAction { .. } | AllocatorAction::Deposit { .. } => {}
        }
    }

    #[test]
    fn drift_mode_inside_rebalance_band_no_action() {
        // multiply target 0.30 vs current 0.295 → drift = -50 bps,
        // inside the default 200-bps band. NoAction even with idle
        // present, because the band is the whole point of having one.
        let s = vec![
            sr("stable_yield", 30.0, 500),
            sr("multiply", 29.5, 1500),
            sr("hedgedjlp", 40.0, 1500),
        ];
        match decide(&s, 100.0, 0.5, &cfg_with_targets(0.30, 0.30, 0.40)) {
            AllocatorAction::NoAction { reason } => {
                assert!(
                    reason.contains("band") || reason.contains("rebalance"),
                    "expected band-related reason, got: {reason}"
                );
            }
            other => panic!("expected NoAction inside band, got {other:?}"),
        }
    }

    #[test]
    fn drift_mode_target_zero_strategy_receives_no_deposit() {
        // Operator wants hedgedjlp at 0 (e.g. while debugging). Target
        // 0.50/0.50/0.0. Current: $0/$0/$0, idle=$100. hedgedjlp would
        // be most-underweight by raw drift (current 0, target 0), but
        // target=0 means it's NOT eligible for deposit. Picker chooses
        // stable_yield (the next-most-underweight DEPLOYABLE).
        let s = vec![
            sr("stable_yield", 0.0, 500),
            sr("multiply", 0.0, 1500),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 100.0, 100.0, &cfg_with_targets(0.50, 0.50, 0.0)) {
            AllocatorAction::Deposit { strategy, .. } => {
                assert_eq!(
                    strategy, "stable_yield",
                    "target=0 strategy should not receive deposit"
                );
            }
            other => panic!("expected Deposit(stable_yield), got {other:?}"),
        }
    }

    #[test]
    fn drift_mode_rc15_incident_shape_still_no_withdraw() {
        // The rc15 incident in drift mode: stable APR 8.96%, multiply
        // 11.61% (above hurdle 10.96), hedgedjlp 10.53% (gap = -143
        // bps, inside the 150-bps hysteresis band). With targets set,
        // the hurdle gate STILL takes precedence and absorbs the noise
        // — must not full-unwind hedgedjlp.
        let s = vec![
            sr("stable_yield", 55.20, 896),
            sr("multiply", 8.33, 1161),
            sr("hedgedjlp", 173.82, 1053),
        ];
        match decide(&s, 239.33, 1.98, &cfg_with_targets(0.30, 0.30, 0.40)) {
            AllocatorAction::Withdraw { .. } => {
                panic!("rc15 regression: drift mode must not Withdraw on hysteresis-band gap")
            }
            AllocatorAction::Deposit { .. } | AllocatorAction::NoAction { .. } => {}
        }
    }

    #[test]
    fn drift_mode_backwards_compat_none_falls_through_to_greedy() {
        // With target_weights: None (the default), decide() must
        // behave exactly like the rc21 greedy path. Reuse the rc15
        // post-unwind shape that the greedy regression test pins.
        let s = vec![
            sr("stable_yield", 55.20, 896),
            sr("multiply", 8.33, 1061), // gap -35, inside band
            sr("hedgedjlp", 0.0, 1040), // not deployed, below hurdle
        ];
        // Default config (no target_weights) — same call shape as the
        // existing rc15_regression_post_unwind_idle_redeploys test.
        let cfg = AllocatorConfig::default();
        match decide(&s, 239.37, 175.84, &cfg) {
            AllocatorAction::Deposit { strategy, .. } => {
                assert_eq!(
                    strategy, "stable_yield",
                    "greedy path must be unchanged when target_weights is None"
                );
            }
            other => panic!("expected greedy Deposit(stable_yield), got {other:?}"),
        }
    }

    #[test]
    fn apr_weighted_mode_deposits_to_highest_gap_strategy() {
        // hedgedjlp has the highest gap (1500 - 1000 = 500), multiply
        // has a smaller gap (1100 - 900 = 200). With APR-weighted
        // dynamic targets, hedgedjlp should get the bulk of the
        // non-stable budget → biggest underweight when current is
        // all-idle → wins the Deposit.
        let s = vec![
            sr("stable_yield", 0.0, 700),
            sr("multiply", 0.0, 1100),
            sr("hedgedjlp", 0.0, 1500),
        ];
        let cfg = AllocatorConfig {
            target_weights: Some(TargetMode::AprWeighted(
                AprWeightedConfig::default(),
            )),
            min_action_usd: 1.0,
            min_drift_bps: 200,
            ..AllocatorConfig::default()
        };
        match decide(&s, 100.0, 100.0, &cfg) {
            AllocatorAction::Deposit { strategy, reason, .. } => {
                assert_eq!(
                    strategy, "hedgedjlp",
                    "highest-gap strategy should win in APR-weighted mode"
                );
                assert!(
                    reason.contains("drift mode"),
                    "audit should label mode: {reason}"
                );
            }
            other => panic!(
                "expected Deposit(hedgedjlp) in APR-weighted mode, got {other:?}"
            ),
        }
    }

    #[test]
    fn apr_weighted_mode_falls_back_to_stable_when_nothing_beats_hurdle() {
        // All non-stable below hurdle → APR-weighted target collapses
        // to all-in-stable. With idle present, deposit goes to
        // stable_yield (which has the largest underweight after the
        // collapse).
        let s = vec![
            sr("stable_yield", 0.0, 700),
            sr("multiply", 0.0, 800), // APR 800 < hurdle 900 → gap 0
            sr("hedgedjlp", 0.0, 800), // APR 800 < hurdle 1000 → gap 0
        ];
        let cfg = AllocatorConfig {
            target_weights: Some(TargetMode::AprWeighted(
                AprWeightedConfig::default(),
            )),
            min_action_usd: 1.0,
            ..AllocatorConfig::default()
        };
        match decide(&s, 100.0, 100.0, &cfg) {
            AllocatorAction::Deposit { strategy, .. } => {
                assert_eq!(
                    strategy, "stable_yield",
                    "all below hurdle → APR-weighted collapses to stable"
                );
            }
            other => panic!("expected Deposit(stable_yield), got {other:?}"),
        }
    }

    #[test]
    fn apr_weighted_targets_react_to_apr_shifts() {
        // Pin the "dynamic" property: same snapshot shape but
        // different APRs → different resolved target vectors. This is
        // the load-bearing M5 contract.
        let cfg = AllocatorConfig {
            target_weights: Some(TargetMode::AprWeighted(
                AprWeightedConfig::default(),
            )),
            ..AllocatorConfig::default()
        };
        // Scenario A: multiply gap 200, hedgedjlp gap 600 (3:1 ratio
        // in hedgedjlp's favour).
        let s_a = vec![
            sr("stable_yield", 0.0, 700),
            sr("multiply", 0.0, 1100),
            sr("hedgedjlp", 0.0, 1600),
        ];
        let mode = cfg.target_weights.as_ref().unwrap();
        let weights_a = mode.resolve(&s_a, &cfg, 700);
        assert!(
            weights_a.hedgedjlp > weights_a.multiply,
            "hedgedjlp gap=600 > multiply gap=200 → hedgedjlp should win: {weights_a:?}"
        );

        // Scenario B: hurdles flip — multiply now has the bigger gap.
        let s_b = vec![
            sr("stable_yield", 0.0, 700),
            sr("multiply", 0.0, 1600),  // APR 1600, gap = 1600 - (700+200) = 700
            sr("hedgedjlp", 0.0, 1100), // APR 1100, gap = 1100 - (700+300) = 100
        ];
        let weights_b = mode.resolve(&s_b, &cfg, 700);
        assert!(
            weights_b.multiply > weights_b.hedgedjlp,
            "multiply gap=700 > hedgedjlp gap=100 → multiply should win: {weights_b:?}"
        );

        // Both scenarios keep the stable_yield_floor.
        assert!((weights_a.stable_yield - 0.20).abs() < 1e-6);
        assert!((weights_b.stable_yield - 0.20).abs() < 1e-6);
    }

    #[test]
    fn drift_mode_zero_aum_no_action_not_panic() {
        // Pathological snapshot: all zero AUM. Drift math would divide
        // by zero; must return a graceful NoAction with an explanatory
        // reason rather than panicking.
        let s = vec![
            sr("stable_yield", 0.0, 500),
            sr("multiply", 0.0, 1500),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 0.0, 0.0, &cfg_with_targets(0.30, 0.30, 0.40)) {
            AllocatorAction::NoAction { reason } => {
                assert!(
                    reason.contains("total_aum") || reason.contains("zero"),
                    "got: {reason}"
                );
            }
            other => panic!("expected NoAction at zero AUM, got {other:?}"),
        }
    }

    #[test]
    fn idle_present_but_caps_below_min_surfaces_pending_note() {
        // Audit follow-up: exercise the step-4 "idle caps below min"
        // path where pending_note has been set. The bug guard is that
        // pending_note must propagate into the audit reason so the
        // operator sees BOTH the under-hurdle observation AND the
        // cap-shrinks-below-min outcome on a single line.
        let s = vec![
            sr("stable_yield", 5.0, 700),
            sr("multiply", 2.0, 100), // gap = -800, clears threshold, but amount<min
        ];
        let c = AllocatorConfig {
            // Tight cap so step-4 idle deposit also gets clamped below min.
            max_action_fraction: 0.05, // 5% of $8 AUM = $0.40
            min_action_usd: 5.0,
            ..AllocatorConfig::default()
        };
        match decide(&s, 8.0, 1.0, &c) {
            AllocatorAction::NoAction { reason } => {
                assert!(
                    reason.contains("multiply"),
                    "pending_note must reference the under-hurdle strategy: {reason}"
                );
                assert!(
                    reason.contains("below min"),
                    "should surface the cap-below-min outcome: {reason}"
                );
            }
            other => panic!("expected NoAction with merged reason, got {other:?}"),
        }
    }
}
