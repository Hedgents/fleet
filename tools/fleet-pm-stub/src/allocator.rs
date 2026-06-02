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
                // rc34 (2026-05-27): non-deployable strategies (multiply
                // — `AssignMultiply` has no `usdc_lamports` field) get
                // their `apr_bps` zeroed for the gap calculation. This
                // forces their gap to ≤ 0, dropping their target share
                // to 0, so the non-stable budget redistributes among
                // strategies the allocator can actually fund.
                //
                // Pre-rc34 behaviour: with multiply APR ~9% (gap ~169
                // bps) dominating the gap-weighted formula, multiply got
                // ~92% of the non-stable target share, but the
                // allocator couldn't deploy to it. hedgedjlp's resulting
                // ~6% target at $264 AUM = ~$16 — below hedgedjlp's
                // $100 desk floor. The rc28 fallback then dumped every
                // rebalance back into stable_yield, defeating the rc29
                // cross-strategy rebalance entirely.
                //
                // Post-rc34: zeroing multiply's apr gives hedgedjlp 100%
                // of the non-stable budget. With stable_yield_floor 0.20,
                // hedgedjlp target = 0.80 × AUM, easily clearing the
                // $100 floor at any non-trivial AUM.
                let inputs: Vec<GapInput<'_>> = strategies
                    .iter()
                    .map(|s| {
                        let hurdle = risk_premium_for(&s.id, cfg)
                            .map(|prem| risk_free_bps.saturating_add(prem))
                            .unwrap_or(0); // stable_yield: hurdle = 0 (itself the anchor)
                        let apr_bps = if is_deployable_via_allocator(&s.id) {
                            s.nominal_apr_bps
                        } else {
                            // Non-deployable strategies (multiply) get a
                            // zero gap regardless of their actual APR.
                            // Their *current deployment* still counts in
                            // the drift math downstream; we're only
                            // saying "don't allocate any future capital
                            // to this slice".
                            0
                        };
                        GapInput {
                            id: &s.id,
                            apr_bps,
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

/// v0.4.13: directional SOL beta of one unit of equity deployed in
/// this strategy. Used by the cross-strategy rebalance cost-benefit
/// gate to credit moves that REDUCE the fleet's net SOL exposure
/// (e.g. multiply → hedgedjlp), alongside the pure APR-gap term.
///
/// Values fixed in code rather than estimated per-tick because
/// per-strategy beta is structural, not market-dependent:
///   - `multiply`: 1.0 — leveraged jitoSOL, NET equity moves 1:1 with
///     SOL (collateral and debt both denominated in SOL, the spread
///     IS the SOL position).
///   - `hedgedjlp`: 0.0 — delta-neutral by design (JLP long + perp
///     shorts sized against the non-stable custody share). Drift away
///     from zero is what the resize loop closes.
///   - `stable_yield`: 0.0 — USDC supply, no SOL exposure.
///   - anything else: 0.0 conservatively (no credit when we don't
///     know).
pub fn sol_beta_for(strategy_id: &str) -> f64 {
    match strategy_id {
        "multiply" => 1.0,
        "hedgedjlp" => 0.0,
        "stable_yield" => 0.0,
        _ => 0.0,
    }
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

    /// rc29: minimum overweight drift (bps of total AUM) needed before
    /// the cross-strategy rebalance path will emit a Withdraw from an
    /// over-deployed strategy when idle is insufficient to fund a
    /// Deposit. Default 1500 bps (15% of AUM above target).
    ///
    /// This is the threshold for ACTIVE rebalance between healthy
    /// strategies. Without this path (pre-rc29) the allocator never
    /// reshuffled capital between strategies both above their hurdles —
    /// once $X landed in stable_yield it stayed there forever even if
    /// hedgedjlp's APR was double.
    ///
    /// Set higher (e.g. 3000 = 30%) to make rebalances rare; lower
    /// (e.g. 800) to track APR-weighted targets more aggressively.
    /// Below `min_drift_bps` the normal rebalance band absorbs first.
    pub rebalance_overweight_bps: i32,

    /// rc29: minimum APR gap (bps) between the overweight strategy and
    /// the best-eligible underweight target needed before the cross-
    /// strategy rebalance path fires. Without this gate the rebalance
    /// could whipsaw against tiny APR-rate noise. Default 200 bps
    /// (2.00%) — well above Kamino's typical APR jitter and large
    /// enough that the gas+slippage cost of a withdraw+deposit cycle
    /// pays back inside a typical holding period. Kept post-rc37 as a
    /// noise floor; the *real* break-even gate is now the cost-benefit
    /// check below.
    pub rebalance_min_apr_gap_bps: i32,

    /// rc37: assumed holding period (days) for the cost-benefit check.
    /// A rebalance only fires if its expected APR gain over this many
    /// days exceeds the open-cost paid up-front (per
    /// `open_cost_bps`). Default 30 days — matches a typical institutional
    /// rebalance cadence and gives slippage + fees time to amortise.
    /// Lower → allocator gets more aggressive (more rebalances, but
    /// some lose money). Higher → allocator gets more patient (fewer
    /// rebalances, more capital sticks in stable strategies).
    pub expected_holding_days: u32,

    /// rc37: safety multiplier on top of the break-even calculation.
    /// `expected_gain ≥ open_cost × cost_safety_factor` is the actual
    /// gate. Default 1.0 (pure break-even). Set above 1 (e.g. 1.5) to
    /// require headroom above pure break-even — useful when expected
    /// APRs are volatile and "future earnings" estimates are noisy.
    pub cost_safety_factor: f64,

    /// v0.4.13: annualised risk-reduction credit (bps per unit of beta
    /// reduction) added to the cost-benefit gate's expected-gain side
    /// for cross-strategy rebalances. Default 2000 bps (20 %/year), a
    /// conservative read on the one-sided downside risk of holding 1×
    /// SOL beta vs delta-neutral.
    ///
    /// Math: `risk_credit_usd = amount × Δβ × (risk_credit_bps_pa /
    /// 10_000) × (holding_days / 365)` where `Δβ = over_beta −
    /// under_beta` (positive when the rebalance moves capital from a
    /// directional strategy to a market-neutral one). This term is
    /// added to the pure-APR `gain_usd` before comparison against the
    /// open-cost gate, so a small APR gap can still pass the cost-
    /// benefit check if it materially reduces directional exposure.
    ///
    /// Only the cross-strategy rebalance path consumes this credit;
    /// the idle → strategy deposit path passes Δβ = 0 (idle has the
    /// same effective beta as wherever the deposit lands? — actually
    /// idle USDC has β=0 too, so depositing to multiply would
    /// INCREASE beta, not reduce. We deliberately don't penalise
    /// that direction here: positive expected APR is still the
    /// primary driver for "deploy idle". Future work can add a
    /// symmetric penalty if needed.
    ///
    /// Set to 0 to disable risk-credit and revert to pure-APR rc37
    /// behaviour.
    pub risk_credit_bps_per_unit_beta_pa: u32,
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
            rebalance_overweight_bps: 1500,
            rebalance_min_apr_gap_bps: 200,
            expected_holding_days: 30,
            cost_safety_factor: 1.0,
            risk_credit_bps_per_unit_beta_pa: 2000,
        }
    }
}

/// rc37 + v0.4.13: cost-benefit check used by both the deposit picker
/// and the cross-strategy rebalance path. Returns `Ok(())` if the
/// proposed move pays back its opening cost within
/// `cfg.expected_holding_days`. Returns `Err(diagnostic_string)`
/// otherwise — the caller stitches the diagnostic into the
/// `NoAction` / fallback reason so the operator can audit why a
/// deposit didn't fire.
///
/// `delta_beta` is the SOL-beta reduction the rebalance achieves
/// (positive when capital moves from a higher-beta strategy to a
/// lower-beta one). Pass `0.0` from the idle → strategy deposit
/// path; pass `over_beta − under_beta` from the cross-strategy
/// rebalance path. A positive `delta_beta` adds a risk-reduction
/// credit to the expected-gain side of the gate.
///
/// Math:
/// ```text
/// apr_gain   = amount × apr_gap × holding_days / (365 × 10_000)
/// risk_gain  = amount × max(delta_beta, 0)
///                × risk_credit_bps_pa × holding_days / (365 × 10_000)
/// cost       = amount × open_cost_bps / 10_000
/// fire when (apr_gain + risk_gain) ≥ cost × safety_factor.
/// ```
fn passes_cost_benefit(
    target_id: &str,
    amount_usd: f64,
    apr_gap_bps: i32,
    delta_beta: f64,
    cfg: &AllocatorConfig,
) -> Result<(), String> {
    let open_cost_bps = open_cost_bps(target_id);
    let cost_usd = amount_usd * (open_cost_bps as f64) / 10_000.0;
    let apr_gain_usd = if apr_gap_bps > 0 {
        amount_usd * (apr_gap_bps as f64) * (cfg.expected_holding_days as f64)
            / (365.0 * 10_000.0)
    } else {
        0.0
    };
    // v0.4.13: risk-reduction credit. Only the rebalance path passes a
    // positive delta_beta; idle → strategy deposits pass 0. The credit
    // is one-sided (max with 0) — we don't penalise moves that
    // INCREASE beta here; that direction is governed by APR and risk-
    // premium gates upstream.
    let risk_gain_usd = if delta_beta > 0.0 && cfg.risk_credit_bps_per_unit_beta_pa > 0 {
        amount_usd
            * delta_beta
            * (cfg.risk_credit_bps_per_unit_beta_pa as f64)
            * (cfg.expected_holding_days as f64)
            / (365.0 * 10_000.0)
    } else {
        0.0
    };
    let gain_usd = apr_gain_usd + risk_gain_usd;
    if gain_usd <= 0.0 {
        return Err(format!(
            "no expected gain (apr_gap {apr_gap_bps} bps, delta_beta {delta_beta:.2})",
        ));
    }
    let required = cost_usd * cfg.cost_safety_factor;
    if gain_usd < required {
        let breakeven_days = if apr_gap_bps > 0 {
            (open_cost_bps as f64) * 365.0 / (apr_gap_bps as f64)
        } else {
            f64::INFINITY
        };
        let risk_note = if risk_gain_usd > 0.0 {
            format!(", +${:.2} risk-credit (Δβ {:.2})", risk_gain_usd, delta_beta)
        } else {
            String::new()
        };
        return Err(format!(
            "expected ${:.2} gain over {}d (apr_gap {} bps{}) < ${:.2} open cost \
             ({} bps × {:.1}x safety) — break-even at {:.0}d hold",
            gain_usd,
            cfg.expected_holding_days,
            apr_gap_bps,
            risk_note,
            required,
            open_cost_bps,
            cfg.cost_safety_factor,
            breakeven_days,
        ));
    }
    Ok(())
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
/// this strategy? All three of `stable_yield`, `hedgedjlp`, and (as of
/// rc40-rc42) `multiply` accept a `usdc_lamports` sizing field on their
/// Assign envelopes.
///
/// For `multiply` specifically, the pipeline is rc40 (envelope shape) +
/// rc41 (daemon-side Jupiter USDC→SOL swap and seed) + rc42 (Deposit
/// emit-path wiring). Below rc41 the daemon rejects non-zero
/// `usdc_lamports`; below rc42 the allocator never populated it.
///
/// Unknown strategies default to deployable — the envelope-spec layer
/// in `allocator_runner.rs` is the second gate that catches truly-
/// unknown ids by bailing on the dispatch.
pub fn is_deployable_via_allocator(_id: &str) -> bool {
    true
}

/// Per-desk minimum deposit size, in USD. Mirrors the hard caps each
/// daemon enforces in its own `caps.rs`:
///
/// - `hedgedjlp`: $100 (`hedgedjlp_daemon::caps::MIN_POSITION_USDC_LAMPORTS`
///   = 100_000_000 micro-USDC). Below this, the daemon's `cap validation`
///   gate rejects the Assign with "sub-$100 doesn't pencil after fixed
///   costs".
/// - `stable_yield`: $1 (`stable_yield_daemon::caps::MIN_POSITION_USDC_LAMPORTS`
///   = 1_000_000). Anything above the Kamino dust-deposit guard.
/// - `multiply`: $10. The daemon has no hard floor in caps.rs, but the
///   Jupiter USDC→SOL swap + Jito stake + Kamino deposit chain costs
///   ~$0.05-0.10 in fees regardless of size, so a $10 floor keeps fee
///   drag under 100 bps. The rc37 cost-benefit gate provides the upper
///   guard (won't fire if expected_gain < cost × safety_factor).
///
/// The allocator uses this in the deposit picker to skip strategies it
/// CAN address but where the daemon would reject the resulting Assign.
/// Without this gate, the allocator gets stuck in a "propose-reject-
/// cooldown" loop every tick — the 2026-05-23 rc27 post-incident found
/// $81 idle while the orchestrator kept proposing `Deposit hedgedjlp
/// $81.09` against a $100 floor.
///
/// NOTE: these values MUST stay in lockstep with each desk's
/// `MIN_POSITION_USDC_LAMPORTS` constant. The desks remain authoritative
/// — this table is an advisory the allocator uses to avoid wasted
/// envelopes, not a security boundary.
pub fn min_deposit_usd(id: &str) -> f64 {
    match id {
        "hedgedjlp" => 100.0,
        "stable_yield" => 1.0,
        "multiply" => 10.0,
        _ => 0.0,
    }
}

/// rc37: per-strategy *opening cost*, in bps of the deposited amount.
/// Captures the up-front costs paid when capital moves INTO this
/// strategy — Jupiter Swap slippage, Jupiter Perps opening fees,
/// Kamino borrow/swap round-trip slippage in the multiply leverage
/// loop, etc. Observed from production cycles on 2026-05-27:
///
/// - `stable_yield`: ~5 bps. Just a Kamino deposit ixn — no AMM
///   slippage. Tx fees + reserve-rounding dust.
/// - `hedgedjlp`: ~40 bps. Jupiter Swap on USDC→JLP (30-80 bps
///   actual slippage at the 150 bps tolerance configured),
///   plus 3 × 0.06% Jupiter Perps opening fees on the SOL/BTC/ETH
///   short legs (~$0.65 on a $360 notional hedge).
/// - `multiply`: ~30 bps. Per-round Jupiter swap on SOL→jitoSOL
///   amortised across 4-6 leverage rounds; opening cost scales
///   with the size of the seed deposit + the rounds_left × slippage
///   stack. Conservative single-shot estimate.
///
/// Used by the allocator to decide whether a proposed rebalance or
/// deposit will *actually* earn back its opening cost over a
/// reasonable holding period — see `AllocatorConfig::expected_holding_days`.
/// Pre-rc37, the allocator only checked APR gap against the static
/// `rebalance_min_apr_gap_bps` floor and would happily emit rebalance
/// envelopes that lost money on slippage + fees before the APR
/// differential paid them back.
pub fn open_cost_bps(id: &str) -> u32 {
    match id {
        "stable_yield" => 5,
        "multiply" => 30,
        "hedgedjlp" => 40,
        _ => 0, // unknown strategies treated as free; caller must skip
    }
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
    if let Some(worst) = levs
        .iter()
        .find(|l| l.s.deployed_usd > 0.0 && l.gap_bps < 0 && -l.gap_bps >= withdraw_threshold_bps)
    {
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
                                              // rc28: gate the deposit candidate on the desk's own
                                              // min-deposit floor. Without this, the allocator can
                                              // happily propose `Deposit hedgedjlp $81` against a $100
                                              // daemon-side floor — the daemon rejects, cooldown resets,
                                              // and the loop repeats every tick (the 2026-05-23 rc27
                                              // post-incident behaviour). The desk remains authoritative;
                                              // `min_deposit_usd` is an advisory mirror.
            let best_deployable = levs.iter().find(|l| {
                l.gap_bps > 0
                    && is_deployable_via_allocator(&l.s.id)
                    && amount >= min_deposit_usd(&l.s.id)
            });
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
                // rc37: cost-benefit gate (gap above hurdle is the
                // economic surplus the operator is paying open-cost
                // to capture; below break-even it isn't worth it).
                // v0.4.13: idle → strategy carries no Δβ (idle has β=0,
                // and we don't credit deposits that INCREASE beta —
                // that direction is governed by APR + risk-premium).
                if let Err(cb_reason) =
                    passes_cost_benefit(&best.s.id, amount, best.gap_bps, 0.0, cfg)
                {
                    reason = format!("{} (skipped: {})", reason, cb_reason);
                    return AllocatorAction::NoAction { reason };
                }
                if let Some(note) = &pending_note {
                    reason = format!("{} (also: {})", reason, note);
                }
                return AllocatorAction::Deposit {
                    strategy: best.s.id.clone(),
                    amount_usd: amount,
                    reason,
                };
            }
            // rc28: surface the reason no leveraged strategy was picked.
            // If the best-gap candidate exists but was skipped because
            // `amount < its min_deposit_usd`, say so in the reason
            // string — operators reading the audit log can then judge
            // whether to top up or accept the stable_yield park.
            let skip_note = levs.iter().find_map(|l| {
                if l.gap_bps > 0
                    && is_deployable_via_allocator(&l.s.id)
                    && amount < min_deposit_usd(&l.s.id)
                {
                    Some(format!(
                        "{} above hurdle but action ${:.2} below desk min ${:.2}",
                        l.s.id,
                        amount,
                        min_deposit_usd(&l.s.id),
                    ))
                } else {
                    None
                }
            });
            // No deployable leveraged above hurdle → park in stable_yield
            // IF the amount clears stable_yield's floor too. (At $1 the
            // floor is rarely binding, but be explicit so a future
            // tightening of that floor doesn't silently break us.)
            if amount >= min_deposit_usd("stable_yield") {
                let mut reason = format!(
                    "no deployable leveraged above hurdle; park idle ${:.2} in stable_yield @ {}",
                    idle_usd,
                    fmt_bps(risk_free),
                );
                if let Some(note) = &skip_note {
                    reason = format!("{} ({})", reason, note);
                }
                if let Some(note) = &pending_note {
                    reason = format!("{} (also: {})", reason, note);
                }
                return AllocatorAction::Deposit {
                    strategy: "stable_yield".to_string(),
                    amount_usd: amount,
                    reason,
                };
            }
            // Both stable_yield and leveraged candidates below their
            // mins: nothing to do this tick. Surface clearly so
            // operators understand the idle parked dust.
            let reason = format!(
                "idle ${:.2} below all desk minimums (stable_yield ${:.2}{}); leaving idle",
                amount,
                min_deposit_usd("stable_yield"),
                skip_note.map(|s| format!(", {}", s)).unwrap_or_default(),
            );
            return AllocatorAction::NoAction { reason };
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
            let eligible = is_deployable_via_allocator(&s.id) && target > 0.0 && above_hurdle;
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
    let most_underweight_bps = rows.first().map(|r| (-r.drift_bps).max(0)).unwrap_or(0);
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
        // rc29: idle isn't enough to fund a deposit, but a meaningful
        // underweight exists. Try the cross-strategy rebalance path:
        // if some OTHER strategy is materially overweight AND its APR
        // is materially worse than the underweight one's, emit a
        // Withdraw to free capital. Next tick the freed USDC becomes
        // idle, and the normal deposit picker routes it correctly.
        if let Some(action) = try_cross_strategy_rebalance(
            strategies,
            total_aum_usd,
            cfg,
            &rows,
            best,
            levs,
            &pending_note,
        ) {
            return action;
        }
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

    // rc28: also gate on the desk's own min-deposit floor. Without
    // this, drift mode can propose a $50 hedgedjlp deposit when the
    // desk floor is $100 — daemon rejects, drift never closes, every
    // tick repeats the same proposal. (Same root cause as the rc27
    // post-incident orchestrator loop on $81 → hedgedjlp.) If the
    // best candidate falls below its desk min, fall back to
    // stable_yield IF the amount clears its floor.
    if amount < min_deposit_usd(&best.id) {
        let desk_min = min_deposit_usd(&best.id);
        if amount >= min_deposit_usd("stable_yield") && best.id != "stable_yield" {
            let mut reason = format!(
                "drift mode: {} underweight by {} bps but action ${:.2} below desk min ${:.2}; \
                 parking ${:.2} in stable_yield",
                best.id, underweight_bps, amount, desk_min, amount,
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
        return no_action_with_drift_summary(
            &rows,
            idle_usd,
            cfg.min_action_usd,
            &format!(
                "drift mode: {} underweight by {} bps but action ${:.2} below desk min ${:.2}",
                best.id, underweight_bps, amount, desk_min,
            ),
            pending_note,
        );
    }

    // rc37: cost-benefit check on the deposit. For idle → strategy, the
    // gap is the strategy's APR over the risk-free 0% idle rate. Tiny
    // deposits into high-fee strategies (e.g. $5 into hedgedjlp where
    // opening cost is 40 bps) shouldn't fire — the slippage swamps the
    // expected gain over any reasonable holding window.
    let target_apr_bps = strategies
        .iter()
        .find(|s| s.id == best.id)
        .map(|s| s.nominal_apr_bps)
        .unwrap_or(0);
    // v0.4.13: idle → strategy deposit; Δβ unused (see passes_cost_benefit
    // docstring).
    if let Err(cb_reason) = passes_cost_benefit(&best.id, amount, target_apr_bps, 0.0, cfg) {
        return no_action_with_drift_summary(
            &rows,
            idle_usd,
            cfg.min_action_usd,
            &format!(
                "drift mode: {} underweight by {} bps but {}",
                best.id, underweight_bps, cb_reason
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

/// rc29: cross-strategy rebalance — withdraw from an overweight
/// strategy when idle is insufficient to fund the most-underweight one.
///
/// Pre-rc29, the drift picker only emitted Deposits sized by `idle_usd`.
/// If idle was ~0 and a deployed strategy was significantly overweight
/// while another deployable one was underweight, the allocator's only
/// reaction was `NoAction` — capital stayed locked in the suboptimal
/// strategy indefinitely. The 2026-05-23 post-rc28 state ($256 in
/// stable_yield @ 5.41% vs $0 in hedgedjlp @ 9.51%) was the canonical
/// example: a 410 bps APR gap × $250 ≈ $10/year of foregone yield,
/// against ~$1-2 of one-time move fees. The math says move; pre-rc29
/// the allocator never offered the move.
///
/// This function fires the move. Conditions ALL must hold:
///   1. An eligible underweight target (the caller's `best`).
///   2. Some strategy is overweight by ≥ `rebalance_overweight_bps`.
///   3. The overweight strategy's APR is at least
///      `rebalance_min_apr_gap_bps` *below* `best`'s APR — without
///      this gate, single-tick APR noise would cause whipsawing.
///   4. The proposed Withdraw amount clears `min_action_usd` and
///      `max_action_fraction` × AUM.
///
/// Returns a `Withdraw` for the overweight strategy. The orchestrator's
/// next tick sees the freed USDC as idle and the regular deposit picker
/// routes it to the underweight target (i.e. the rebalance completes
/// in two ticks, not one — by design, so the on-chain settlement of
/// the withdraw lands before the deposit is signed).
fn try_cross_strategy_rebalance(
    strategies: &[StrategyRate],
    total_aum_usd: f64,
    cfg: &AllocatorConfig,
    rows: &[DriftRow],
    best: &DriftRow,
    levs: &[LevGap<'_>],
    pending_note: &Option<String>,
) -> Option<AllocatorAction> {
    // Find the most-overweight strategy with non-zero deployment.
    // drift_bps > 0 = overweight (current > target). Sort descending.
    let mut overweights: Vec<&DriftRow> = rows
        .iter()
        .filter(|r| r.drift_bps > 0 && r.id != best.id)
        .collect();
    overweights.sort_by_key(|r| -r.drift_bps);

    let over = overweights.first()?;
    if over.drift_bps < cfg.rebalance_overweight_bps {
        // Not overweight enough — would churn against noise.
        return None;
    }

    // Resolve current APRs from the strategies snapshot to gate on
    // the APR gap (best - over). best.id is the target underweight;
    // over.id is the candidate to withdraw from.
    let underweight_apr = strategies
        .iter()
        .find(|s| s.id == best.id)
        .map(|s| s.nominal_apr_bps)?;
    let overweight_apr = strategies
        .iter()
        .find(|s| s.id == over.id)
        .map(|s| s.nominal_apr_bps)?;
    let apr_gap = underweight_apr.saturating_sub(overweight_apr);
    // v0.4.13: compute the Δβ once; both the apr-gap floor (below) and
    // the cost-benefit gate (later) consult it. Positive Δβ = the
    // rebalance moves capital from a higher-beta strategy to a
    // lower-beta one (risk-reducing). Half a unit (0.5) is the cutoff
    // for bypassing the noise-floor apr-gap check — anything smaller
    // we treat as a pure-APR rebalance.
    let delta_beta = sol_beta_for(&over.id) - sol_beta_for(&best.id);
    if apr_gap < cfg.rebalance_min_apr_gap_bps && delta_beta < 0.5 {
        // APRs too close and the move isn't materially risk-reducing —
        // gas+slippage on the round-trip would eat the upside. Stay
        // put. (When Δβ ≥ 0.5 the cost-benefit gate below takes over
        // and can pass on the risk-credit alone.)
        return None;
    }

    // Sanity check that `best` is actually eligible to receive (above
    // hurdle and deployable via allocator). The drift-step caller
    // already filtered on `eligible`, but be explicit.
    if !best.eligible {
        return None;
    }
    let _ = levs; // kept in the signature for future use (e.g. gating
                  // on multi-step hurdle chains); silence unused-var
                  // warning without dropping the parameter.

    // Size the withdraw: bring the overweight strategy from
    // `over.drift_bps` of AUM closer to target. Take half the
    // overweight slice as a damping factor (avoid overshooting if
    // weights drift back after settlement). Then cap by
    // max_action_fraction and floor by min_action_usd.
    let overweight_dollars = (over.drift_bps as f64 / 10_000.0) * total_aum_usd;
    let damp = 0.5;
    let proposed = overweight_dollars * damp;
    let cap = cap_to_aum_fraction(total_aum_usd, cfg.max_action_fraction);
    let deployed = strategies
        .iter()
        .find(|s| s.id == over.id)
        .map(|s| s.deployed_usd)
        .unwrap_or(0.0);
    let amount = proposed.min(cap).min(deployed);

    if amount < cfg.min_action_usd {
        return None;
    }

    // rc37 + v0.4.13: cost-benefit gate. The combined opening cost
    // for the *round-trip* is: withdraw from `over` (~free for
    // stable_yield, ~5 bps for multiply unwind) PLUS deposit to
    // `best` (where the bulk of slippage + fees live — JLP swap +
    // perp opens for hedgedjlp, leverage-loop slippage for multiply).
    // The withdraw side is small enough that we only model the
    // destination cost; the safety factor handles the residual. Δβ
    // adds a risk-reduction credit when the rebalance moves capital
    // from a directional strategy to a market-neutral one, even when
    // the APR gap alone wouldn't clear the gate.
    if let Err(reason) = passes_cost_benefit(&best.id, amount, apr_gap, delta_beta, cfg) {
        // Suppress this rebalance — the cost-benefit math says it
        // doesn't pay back inside the expected holding period.
        // Caller falls through to NoAction with an explanation.
        let _ = reason; // logged via the no-action audit summary, not panicked
        return None;
    }

    let mut reason = format!(
        "rc29 cross-strategy rebalance: {} overweight by {} bps ({}); \
         withdraw ${:.2} to free capital for {} ({} → {}, gap +{} bps)",
        over.id,
        over.drift_bps,
        fmt_bps(overweight_apr),
        amount,
        best.id,
        fmt_bps(overweight_apr),
        fmt_bps(underweight_apr),
        apr_gap,
    );
    if let Some(note) = pending_note {
        reason = format!("{} (also: {})", reason, note);
    }

    Some(AllocatorAction::Withdraw {
        strategy: over.id.clone(),
        amount_usd: amount,
        reason,
    })
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
        // rc37: tests that aren't specifically exercising the cost-benefit
        // gate use a long expected_holding_days so the gate effectively
        // always passes. Specific cost-benefit tests build their own
        // AllocatorConfig with the production default (30 days).
        AllocatorConfig {
            expected_holding_days: 365 * 10, // 10-year hold → gate is always permissive
            ..AllocatorConfig::default()
        }
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
    fn multiply_above_hurdle_with_idle_picked_when_deployable_post_rc42() {
        // rc42: multiply is now deployable (rc41 daemon-side Jupiter
        // USDC→SOL swap landed). With the largest APR gap (+500 above
        // the 700 hurdle) and the picker free to pick it, idle USDC
        // routes to multiply instead of falling back to stable_yield.
        //
        // Pre-rc42 this scenario had a fall-through-to-stable_yield
        // assertion documented as the bug rc42 fixes.
        let s = vec![
            sr("stable_yield", 500.0, 700),
            sr("multiply", 500.0, 1200),
            sr("hedgedjlp", 0.0, 800),
        ];
        match decide(&s, 1050.0, 50.0, &cfg()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                ..
            } => {
                assert_eq!(strategy, "multiply");
                assert!((amount_usd - 50.0).abs() < 1e-9);
            }
            other => panic!("expected Deposit to multiply, got {other:?}"),
        }
    }

    #[test]
    fn highest_gap_strategy_wins_after_rc42_multiply_deployable() {
        // rc42: multiply is now deployable. With the highest APR gap
        // (1500 vs hedgedjlp's 1300), the picker should pick multiply.
        // Pre-rc42 this test asserted "filter picks hedgedjlp because
        // multiply is non-deployable" — that filter no longer applies.
        let s = vec![
            sr("stable_yield", 100.0, 700),
            sr("multiply", 100.0, 1500),
            sr("hedgedjlp", 100.0, 1300),
        ];
        match decide(&s, 500.0, 250.0, &cfg()) {
            AllocatorAction::Deposit { strategy, .. } => assert_eq!(strategy, "multiply"),
            other => panic!("expected Deposit(multiply), got {other:?}"),
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
    fn rc27_post_incident_81_idle_falls_back_to_stable_yield() {
        // 2026-05-23 rc27 post-incident: $81 idle, hedgedjlp has the
        // best gap but its desk floor is $100. Pre-rc28 the allocator
        // kept proposing `Deposit hedgedjlp $81` every tick and the
        // daemon kept rejecting it. With rc28, the picker recognises
        // $81 < hedgedjlp's $100 floor and falls back to stable_yield.
        let s = vec![
            sr("stable_yield", 175.0, 583),
            sr("multiply", 8.0, 776),
            sr("hedgedjlp", 0.0, 1003),
        ];
        // total AUM 183 → max_action_fraction 0.5 caps to $91.5; idle
        // is $81 so amount = $81 (idle is the binding constraint).
        match decide(&s, 183.0, 81.0, &cfg()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                reason,
            } => {
                assert_eq!(
                    strategy, "stable_yield",
                    "must fall back to stable_yield when amount < hedgedjlp floor"
                );
                assert!((amount_usd - 81.0).abs() < 1e-6);
                assert!(
                    reason.contains("hedgedjlp above hurdle but action")
                        && reason.contains("below desk min"),
                    "reason should explain why hedgedjlp was skipped, got: {reason}"
                );
            }
            other => panic!("expected Deposit(stable_yield) as rc28 fallback, got {other:?}"),
        }
    }

    #[test]
    fn amount_above_hedgedjlp_floor_picks_hedgedjlp() {
        // Same fleet shape as the rc27 incident, but $150 idle (above
        // the $100 hedgedjlp floor) → picker now selects hedgedjlp,
        // not the fallback. Guards against rc28 over-applying the
        // gate.
        let s = vec![
            sr("stable_yield", 175.0, 583),
            sr("multiply", 8.0, 776),
            sr("hedgedjlp", 0.0, 1003),
        ];
        // total 183, idle 150, cap 0.5*183 = $91.5 → amount = $91.5.
        // Still below $100 floor → falls back.
        match decide(&s, 183.0, 150.0, &cfg()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                ..
            } => {
                assert_eq!(strategy, "stable_yield");
                assert!((amount_usd - 91.5).abs() < 1e-6);
            }
            other => panic!("expected Deposit, got {other:?}"),
        }
        // With AUM = $300 → cap = $150 → amount = idle $150 > $100
        // floor → hedgedjlp wins.
        match decide(&s, 300.0, 150.0, &cfg()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                ..
            } => {
                assert_eq!(strategy, "hedgedjlp");
                assert!((amount_usd - 150.0).abs() < 1e-6);
            }
            other => panic!("expected Deposit(hedgedjlp), got {other:?}"),
        }
    }

    #[test]
    fn min_deposit_usd_table_matches_desk_constants() {
        // Cross-crate invariant: hedgedjlp_daemon::caps::MIN_POSITION_USDC_LAMPORTS
        // == 100_000_000 (= $100). stable_yield is 1_000_000 (= $1).
        // If this assert fails, the desk constant changed and the
        // table at the top of allocator.rs is stale.
        assert_eq!(min_deposit_usd("hedgedjlp"), 100.0);
        assert_eq!(min_deposit_usd("stable_yield"), 1.0);
        // rc42: multiply allocator floor — Jupiter swap + Jito stake +
        // Kamino deposit chain costs ~$0.05-0.10 in fees regardless of
        // size, so $10 keeps fee drag under 100 bps.
        assert_eq!(min_deposit_usd("multiply"), 10.0);
        assert_eq!(min_deposit_usd("unknown"), 0.0);
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
        // rc42: multiply is now deployable (rc40 wire format + rc41
        // daemon-side Jupiter USDC→SOL swap unlocked allocator routing).
        assert!(is_deployable_via_allocator("multiply"));
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
            // rc37: see `cfg()` — permissive holding period so the
            // cost-benefit gate doesn't preempt drift-picker tests.
            expected_holding_days: 365 * 10,
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
        // hedgedjlp=$0, idle=$500. hedgedjlp has the largest *absolute*
        // underweight (-4000 bps) → wins. rc28: AUM/idle bumped from
        // $100 to $500 so the picked amount clears hedgedjlp's $100
        // desk floor (target test is the drift picker, not the floor).
        let s = vec![
            sr("stable_yield", 0.0, 500),
            sr("multiply", 0.0, 1500),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 500.0, 500.0, &cfg_with_targets(0.30, 0.30, 0.40)) {
            AllocatorAction::Deposit {
                strategy, reason, ..
            } => {
                assert_eq!(strategy, "hedgedjlp", "biggest underweight should win");
                assert!(
                    reason.contains("drift mode"),
                    "audit should label mode: {reason}"
                );
                assert!(
                    reason.contains("underweight"),
                    "audit should name direction: {reason}"
                );
            }
            other => panic!("expected Deposit to hedgedjlp, got {other:?}"),
        }
    }

    #[test]
    fn drift_mode_picks_most_underweight_deployable_strategy_rc42() {
        // rc42: with multiply now deployable, the most-underweight
        // strategy (multiply at target 0.50, current 0.0) wins. Pre-
        // rc42 this test asserted hedgedjlp won because multiply was
        // excluded; that exclusion no longer applies.
        let s = vec![
            sr("stable_yield", 0.0, 500),
            sr("multiply", 0.0, 1500),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 500.0, 500.0, &cfg_with_targets(0.10, 0.50, 0.40)) {
            AllocatorAction::Deposit { strategy, .. } => {
                assert_eq!(strategy, "multiply", "multiply wins as most underweight");
            }
            other => panic!("expected Deposit(multiply), got {other:?}"),
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
            sr("multiply", 0.0, 1500), // above hurdle AND deployable post-rc42
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
            target_weights: Some(TargetMode::AprWeighted(AprWeightedConfig::default())),
            min_action_usd: 1.0,
            min_drift_bps: 200,
            ..AllocatorConfig::default()
        };
        // rc28: AUM/idle bumped to $500 so the picked amount clears
        // hedgedjlp's $100 desk floor. The target test is APR-weighted
        // target resolution, not the floor.
        match decide(&s, 500.0, 500.0, &cfg) {
            AllocatorAction::Deposit {
                strategy, reason, ..
            } => {
                assert_eq!(
                    strategy, "hedgedjlp",
                    "highest-gap strategy should win in APR-weighted mode"
                );
                assert!(
                    reason.contains("drift mode"),
                    "audit should label mode: {reason}"
                );
            }
            other => panic!("expected Deposit(hedgedjlp) in APR-weighted mode, got {other:?}"),
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
            sr("multiply", 0.0, 800),  // APR 800 < hurdle 900 → gap 0
            sr("hedgedjlp", 0.0, 800), // APR 800 < hurdle 1000 → gap 0
        ];
        let cfg = AllocatorConfig {
            target_weights: Some(TargetMode::AprWeighted(AprWeightedConfig::default())),
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
        //
        // rc42 (2026-05-29): multiply is now deployable via allocator,
        // so the rc34 fallback that zeroed its apr_bps in the resolver
        // no longer fires (is_deployable_via_allocator always returns
        // true). Multiply NOW gets gap-weighted share of the non-stable
        // budget alongside hedgedjlp. This test exercises three regimes:
        // (A) both above hurdle → both get share, (B) only hedgedjlp
        // above hurdle → hedgedjlp captures non-stable, (C) neither
        // above hurdle → all-in-stable.
        let cfg = AllocatorConfig {
            target_weights: Some(TargetMode::AprWeighted(AprWeightedConfig::default())),
            ..AllocatorConfig::default()
        };
        let mode = cfg.target_weights.as_ref().unwrap();

        // Scenario A: both above hurdle. Multiply gap = 1100-869 = 231,
        // hedgedjlp gap = 1600-1000 = 600 → hedgedjlp gets the larger
        // share but multiply also gets a non-zero slice.
        let s_a = vec![
            sr("stable_yield", 0.0, 700),
            sr("multiply", 0.0, 1100),
            sr("hedgedjlp", 0.0, 1600),
        ];
        let weights_a = mode.resolve(&s_a, &cfg, 700);
        assert!(
            weights_a.multiply > 0.0,
            "rc42: multiply gets non-zero share when above hurdle: {weights_a:?}"
        );
        assert!(
            weights_a.hedgedjlp > weights_a.multiply,
            "hedgedjlp has larger gap → larger share: {weights_a:?}"
        );

        // Scenario B: hedgedjlp's APR drops below its hurdle (700+300).
        // Only multiply (gap above hurdle) gets the non-stable budget.
        let s_b = vec![
            sr("stable_yield", 0.0, 700),
            sr("multiply", 0.0, 1600),
            sr("hedgedjlp", 0.0, 900), // 900 < (700+300) hurdle
        ];
        let weights_b = mode.resolve(&s_b, &cfg, 700);
        assert_eq!(weights_b.hedgedjlp, 0.0);
        assert!(
            weights_b.multiply > 0.5,
            "only deployable above-hurdle strategy captures non-stable: {weights_b:?}"
        );

        // Scenario A keeps the explicit stable_yield_floor.
        assert!((weights_a.stable_yield - 0.20).abs() < 1e-6);
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

    // ── rc29 cross-strategy rebalance tests ─────────────────────────────

    fn cfg_apr_weighted_rc29() -> AllocatorConfig {
        AllocatorConfig {
            target_weights: Some(TargetMode::AprWeighted(AprWeightedConfig::default())),
            min_action_usd: 5.0,
            min_drift_bps: 200,
            rebalance_overweight_bps: 1500,
            rebalance_min_apr_gap_bps: 200,
            // rc37: see `cfg()` — permissive holding period so the
            // cost-benefit gate doesn't preempt picker-logic tests.
            expected_holding_days: 365 * 10,
            ..AllocatorConfig::default()
        }
    }

    #[test]
    fn rc34_mid_rebalance_state_routes_idle_post_rc42() {
        // 2026-05-27 live production snapshot during the rebalance loop:
        // stable_yield 6.19% holding $135.80, multiply 9.88% holding
        // $8.29, hedgedjlp 9.67% holding $0, idle $120.64.
        //
        // rc42 changes the answer for this scenario. Pre-rc42 (rc34
        // through rc41), multiply was non-deployable and hedgedjlp
        // captured the non-stable budget. With rc42 making multiply
        // deployable AND it having the highest APR gap (988 - 700 ≈
        // 288 bps vs hedgedjlp 967 - 1000 = -33 below its 1000 hurdle),
        // the picker now routes to multiply.
        let s = vec![
            sr("stable_yield", 135.80, 619),
            sr("multiply", 8.29, 988),
            sr("hedgedjlp", 0.0, 967),
        ];
        match decide(&s, 264.73, 120.64, &cfg_apr_weighted_rc29()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                ..
            } => {
                assert_eq!(
                    strategy, "multiply",
                    "rc42: multiply now deployable + highest gap → captures the deposit"
                );
                assert!(
                    amount_usd >= 10.0,
                    "amount must clear multiply's $10 floor: ${amount_usd:.2}"
                );
            }
            other => panic!("expected Deposit(multiply), got {other:?} — rc42 wiring didn't take"),
        }
    }

    #[test]
    fn rc29_post_rc28_state_triggers_cross_strategy_withdraw() {
        // 2026-05-23 actual production snapshot: stable_yield 5.41%
        // holding $256, hedgedjlp 9.51% holding $0, idle ~$0. Pre-rc29
        // this was NoAction forever. rc29: cross-strategy rebalance
        // should withdraw a slice of stable_yield to free idle for
        // the next-tick deposit to hedgedjlp.
        let s = vec![
            sr("stable_yield", 256.0, 541),
            sr("multiply", 8.0, 761),
            sr("hedgedjlp", 0.0, 951),
        ];
        match decide(&s, 264.0, 0.09, &cfg_apr_weighted_rc29()) {
            AllocatorAction::Withdraw {
                strategy,
                amount_usd,
                reason,
            } => {
                assert_eq!(
                    strategy, "stable_yield",
                    "rebalance must withdraw from the overweight strategy"
                );
                assert!(
                    amount_usd > 5.0 && amount_usd <= 132.0,
                    "amount should be sensibly damped from full overweight: ${amount_usd:.2}"
                );
                assert!(
                    reason.contains("rc29 cross-strategy rebalance"),
                    "audit reason must label the new path: {reason}"
                );
                assert!(
                    reason.contains("stable_yield overweight") && reason.contains("hedgedjlp"),
                    "reason must name both sides: {reason}"
                );
            }
            other => panic!("expected cross-strategy Withdraw, got {other:?}"),
        }
    }

    #[test]
    fn rc29_apr_gap_below_threshold_does_not_rebalance() {
        // hedgedjlp 9.00%, stable 5.00% → 400 bps gap. Configure the
        // gate at 500 bps → gap fails the gate even though hedgedjlp
        // is above its hurdle (5%+3%=8% < 9.00% so eligible) and
        // stable_yield is overweight. Tests that the APR-gap gate is
        // independent of the hurdle gate. Without this gate, any
        // above-hurdle underweight would justify the round-trip; with
        // it, operators can require a meaningful additional margin.
        let cfg = AllocatorConfig {
            target_weights: Some(TargetMode::AprWeighted(AprWeightedConfig::default())),
            min_action_usd: 5.0,
            min_drift_bps: 200,
            rebalance_overweight_bps: 1500,
            rebalance_min_apr_gap_bps: 500,
            ..AllocatorConfig::default()
        };
        let s = vec![
            sr("stable_yield", 256.0, 500),
            sr("multiply", 8.0, 800), // at hurdle exactly, not overweight
            sr("hedgedjlp", 0.0, 900),
        ];
        match decide(&s, 264.0, 0.0, &cfg) {
            AllocatorAction::Withdraw { reason, .. } => {
                assert!(
                    !reason.contains("rc29 cross-strategy"),
                    "should NOT cross-rebalance when APR gap < gate: {reason}"
                );
            }
            AllocatorAction::NoAction { .. } | AllocatorAction::Deposit { .. } => {}
        }
    }

    #[test]
    fn rc29_rebalance_only_fires_when_idle_below_min_action() {
        // Same shape as the production snapshot but with $50 idle
        // (well above min_action_usd $5). The normal deposit picker
        // should fire instead of the rebalance path — we don't want
        // BOTH to act in the same tick.
        let s = vec![
            sr("stable_yield", 200.0, 541),
            sr("multiply", 8.0, 761),
            sr("hedgedjlp", 0.0, 951),
        ];
        match decide(&s, 258.0, 50.0, &cfg_apr_weighted_rc29()) {
            AllocatorAction::Deposit { strategy, .. } => {
                // Falls back to stable_yield because $50 < hedgedjlp's
                // $100 floor (rc28 gate). Either way: rebalance path
                // should NOT have fired — idle was enough.
                assert!(
                    strategy == "stable_yield" || strategy == "hedgedjlp",
                    "expected normal deposit path, got Deposit({strategy})"
                );
            }
            AllocatorAction::Withdraw { reason, .. } => {
                if reason.contains("rc29 cross-strategy") {
                    panic!("rebalance fired when idle was sufficient: {reason}");
                }
            }
            AllocatorAction::NoAction { .. } => {} // acceptable
        }
    }

    #[test]
    fn rc29_rebalance_skips_when_overweight_below_threshold() {
        // High threshold (5000 bps = 50%) means even a 3000 bps
        // overweight doesn't trip. With the actual production-shape
        // scenario but the operator dialed the gate up to "only fire
        // on huge drifts", the rebalance must stay quiet. This
        // documents the threshold's role as the operator's veto knob.
        let cfg = AllocatorConfig {
            target_weights: Some(TargetMode::AprWeighted(AprWeightedConfig::default())),
            min_action_usd: 5.0,
            min_drift_bps: 200,
            rebalance_overweight_bps: 5000, // requires 50% overweight
            rebalance_min_apr_gap_bps: 200,
            ..AllocatorConfig::default()
        };
        let s = vec![
            sr("stable_yield", 100.0, 541),
            sr("multiply", 50.0, 761),
            sr("hedgedjlp", 50.0, 951),
        ];
        match decide(&s, 200.0, 0.0, &cfg) {
            AllocatorAction::Withdraw { reason, .. } => {
                assert!(
                    !reason.contains("rc29 cross-strategy"),
                    "rebalance should not fire below overweight threshold: {reason}"
                );
            }
            AllocatorAction::NoAction { .. } | AllocatorAction::Deposit { .. } => {}
        }
    }

    #[test]
    fn rc29_two_tick_settlement_idempotent_after_withdraw() {
        // After the rebalance Withdraw fires (tick 1), tick 2 sees the
        // freed USDC as idle. Simulate the post-withdraw snapshot:
        // stable_yield 200 (was 256, withdrew ~56), idle = 56,
        // hedgedjlp 0. The deposit picker should now route to
        // hedgedjlp. This verifies the two-tick rebalance converges.
        let s = vec![
            sr("stable_yield", 200.0, 541),
            sr("multiply", 8.0, 761),
            sr("hedgedjlp", 0.0, 951),
        ];
        match decide(&s, 264.0, 56.0, &cfg_apr_weighted_rc29()) {
            AllocatorAction::Deposit {
                strategy,
                amount_usd,
                ..
            } => {
                // $56 idle exceeds the $100 hedgedjlp floor? No, it's
                // below — so rc28 fallback routes to stable_yield.
                // That's correct behaviour: tick 1 freed $56, tick 2
                // routes it to stable_yield because it's below the
                // hedgedjlp floor. To actually fund hedgedjlp we'd
                // need at least one Withdraw of $100+. The damp factor
                // (0.5) keeps single-tick moves smaller; multiple
                // rebalance ticks accumulate.
                assert!(
                    strategy == "stable_yield" || strategy == "hedgedjlp",
                    "tick 2 should deposit somewhere, got {strategy}"
                );
                assert!(amount_usd > 0.0);
            }
            other => panic!("expected Deposit on tick 2, got {other:?}"),
        }
    }

    // ── rc37 cost-benefit gate tests ────────────────────────────────────

    fn cfg_rc37_default() -> AllocatorConfig {
        // Production-shape config: 30-day holding window, 1.0x safety
        // factor (pure break-even). Exercises the gate as deployed.
        AllocatorConfig {
            target_weights: Some(TargetMode::AprWeighted(AprWeightedConfig::default())),
            min_action_usd: 1.0,
            min_drift_bps: 200,
            // expected_holding_days defaults to 30 in AllocatorConfig::default()
            ..AllocatorConfig::default()
        }
    }

    #[test]
    fn rc37_open_cost_bps_table_pins_production_values() {
        // Source of truth for deployed open-cost estimates. If anyone
        // tunes these constants, CI shows a diff before it ships.
        assert_eq!(open_cost_bps("stable_yield"), 5);
        assert_eq!(open_cost_bps("multiply"), 30);
        assert_eq!(open_cost_bps("hedgedjlp"), 40);
        assert_eq!(open_cost_bps("unknown_strategy"), 0);
    }

    #[test]
    fn rc37_passes_cost_benefit_blocks_below_breakeven() {
        // hedgedjlp open cost 40 bps. 30-day hold needs gap ≥ 487 bps
        // (40 × 365 / 30). We give 300 bps and expect Err with a
        // diagnostic that names the break-even.
        let cfg = AllocatorConfig {
            expected_holding_days: 30,
            cost_safety_factor: 1.0,
            ..AllocatorConfig::default()
        };
        let result = passes_cost_benefit("hedgedjlp", 1_000.0, 300, 0.0, &cfg);
        assert!(result.is_err(), "300 bps × 30d < 40 bps cost — must block");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("break-even"),
            "diagnostic missing break-even: {msg}"
        );
    }

    #[test]
    fn rc37_passes_cost_benefit_allows_above_breakeven() {
        // 1000 bps gap × 30 / 365 = 82 bps gain ≥ 40 bps cost. Passes.
        let cfg = AllocatorConfig {
            expected_holding_days: 30,
            cost_safety_factor: 1.0,
            ..AllocatorConfig::default()
        };
        assert!(passes_cost_benefit("hedgedjlp", 1_000.0, 1000, 0.0, &cfg).is_ok());
    }

    #[test]
    fn rc37_passes_cost_benefit_safety_factor_raises_bar() {
        // Safety 2.0 doubles the required gain.
        let cfg = AllocatorConfig {
            expected_holding_days: 30,
            cost_safety_factor: 2.0,
            ..AllocatorConfig::default()
        };
        // 1000 bps × 30 / 365 = 82 bps. Required = 40 × 2 = 80. Barely passes.
        assert!(passes_cost_benefit("hedgedjlp", 1_000.0, 1000, 0.0, &cfg).is_ok());
        // 800 bps × 30 / 365 = 65.7 bps. Required = 80. Blocks.
        assert!(passes_cost_benefit("hedgedjlp", 1_000.0, 800, 0.0, &cfg).is_err());
    }

    #[test]
    fn rc37_passes_cost_benefit_zero_and_negative_gap_block() {
        let cfg = cfg_rc37_default();
        assert!(passes_cost_benefit("hedgedjlp", 1_000.0, 0, 0.0, &cfg).is_err());
        assert!(passes_cost_benefit("hedgedjlp", 1_000.0, -100, 0.0, &cfg).is_err());
    }

    #[test]
    fn rc37_passes_cost_benefit_stable_yield_lower_threshold() {
        // stable_yield open cost 5 bps → break-even at 5 × 365 / 30 ≈ 61 bps.
        let cfg = AllocatorConfig {
            expected_holding_days: 30,
            cost_safety_factor: 1.0,
            ..AllocatorConfig::default()
        };
        assert!(passes_cost_benefit("stable_yield", 100.0, 100, 0.0, &cfg).is_ok());
        assert!(passes_cost_benefit("stable_yield", 100.0, 50, 0.0, &cfg).is_err());
    }

    // ── v0.4.13: risk-credit term ───────────────────────────────────────

    #[test]
    fn rc13_sol_beta_table_pins_strategy_assignments() {
        // Source of truth for per-strategy directional exposure. If
        // anyone tunes these, CI shows a diff before it ships.
        assert_eq!(sol_beta_for("multiply"), 1.0);
        assert_eq!(sol_beta_for("hedgedjlp"), 0.0);
        assert_eq!(sol_beta_for("stable_yield"), 0.0);
        // Unknown strategies default to 0 (no credit, no penalty).
        assert_eq!(sol_beta_for("unknown_strategy"), 0.0);
    }

    #[test]
    fn rc13_risk_credit_passes_small_apr_gap_when_beta_drops() {
        // The canonical production scenario the rc-13 fix unblocks:
        // moving $140 from multiply (β=1) to hedgedjlp (β=0) at a tiny
        // APR gap of 148 bps. Pre-rc13 cost-benefit math:
        //   apr_gain = 140 × 1.48% × 30/365 = $0.17
        //   cost     = 140 × 40bps         = $0.56  → block
        // Post-rc13 with Δβ=1.0 and default risk_credit_bps_pa=2000:
        //   risk_gain = 140 × 1.0 × 20% × 30/365 = $2.30
        //   total     = $0.17 + $2.30 = $2.47    > $0.56 → fire
        let cfg = AllocatorConfig {
            expected_holding_days: 30,
            cost_safety_factor: 1.0,
            ..AllocatorConfig::default()
        };
        assert!(
            passes_cost_benefit("hedgedjlp", 140.0, 148, 1.0, &cfg).is_ok(),
            "Δβ=1 risk credit must unblock the canonical multiply→hedgedjlp move"
        );
        // Sanity: same scenario with Δβ=0 (pre-rc13 semantics) still blocks.
        assert!(
            passes_cost_benefit("hedgedjlp", 140.0, 148, 0.0, &cfg).is_err(),
            "without risk credit the move should still fail (rc37 semantics)"
        );
    }

    #[test]
    fn rc13_risk_credit_does_not_credit_increased_beta() {
        // Moving INTO a higher-beta strategy passes Δβ < 0; the credit
        // term clamps at 0 (one-sided), so this should behave exactly
        // like rc37 pure-APR. 148 bps × 30/365 = 1.22 bps gain → ~$0.17
        // on $140 < $0.42 cost (multiply 30 bps) → block.
        let cfg = AllocatorConfig {
            expected_holding_days: 30,
            cost_safety_factor: 1.0,
            ..AllocatorConfig::default()
        };
        assert!(
            passes_cost_benefit("multiply", 140.0, 148, -1.0, &cfg).is_err(),
            "negative Δβ must not credit; rc37 pure-APR semantics preserved"
        );
    }

    #[test]
    fn rc13_risk_credit_disabled_when_config_zero() {
        // Operator can disable risk-credit by setting the config to 0;
        // we revert to pure-APR rc37 behaviour.
        let cfg = AllocatorConfig {
            expected_holding_days: 30,
            cost_safety_factor: 1.0,
            risk_credit_bps_per_unit_beta_pa: 0,
            ..AllocatorConfig::default()
        };
        // Same canonical scenario; with the credit disabled it blocks.
        assert!(passes_cost_benefit("hedgedjlp", 140.0, 148, 1.0, &cfg).is_err());
    }

    #[test]
    fn rc13_risk_credit_diagnostic_names_delta_beta() {
        // When the gate blocks AND a risk credit was applied (but
        // wasn't enough), the diagnostic must surface Δβ so the
        // operator can see the math without re-running the function.
        // 50 bps × 30/365 = 4.1 bps; on $50: $0.02 apr gain.
        // Risk: $50 × 1.0 × 20% × 30/365 = $0.82.
        // Total: $0.84 > $0.20 cost (40 bps × $50). Actually passes.
        // Use smaller amount to make it fail visibly: $10 × 1.0 × 20%
        // × 30/365 = $0.16; APR gain $0.004; cost $10 × 0.4% = $0.04.
        // Still passes. Let's force a fail with very small amount and
        // tiny credit config.
        let cfg = AllocatorConfig {
            expected_holding_days: 30,
            cost_safety_factor: 1.0,
            // Just enough credit to register in the diagnostic, not
            // enough to clear cost.
            risk_credit_bps_per_unit_beta_pa: 10,
            ..AllocatorConfig::default()
        };
        // hedgedjlp cost = 40 bps × $1000 = $4. Credit: 1000 × 1 × 0.1% × 30/365 ≈ $0.008.
        // APR gain: 1000 × 0.5% × 30/365 ≈ $0.41. Total < $4 → fail.
        let res = passes_cost_benefit("hedgedjlp", 1_000.0, 50, 1.0, &cfg);
        assert!(res.is_err());
        let msg = res.unwrap_err();
        assert!(
            msg.contains("risk-credit") && msg.contains("Δβ"),
            "diagnostic must name the risk-credit term and Δβ: {msg}"
        );
    }
}
