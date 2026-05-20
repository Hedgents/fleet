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

use serde::{Deserialize, Serialize};

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
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self {
            risk_premium_bps_multiply: 200,
            risk_premium_bps_hedgedjlp: 300,
            min_action_usd: 5.0,
            max_action_fraction: 0.5,
            min_withdraw_gap_bps: 150,
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

    // Classify leveraged strategies by their gap to hurdle. Negative gap
    // = under-hurdle (candidate for Withdraw). Positive gap = above
    // hurdle (candidate for Deposit).
    struct LevGap<'a> {
        s: &'a StrategyRate,
        hurdle_bps: i32,
        gap_bps: i32, // nominal - hurdle
    }
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
    if let Some(worst) = levs.iter().find(|l| {
        l.s.deployed_usd > 0.0
            && l.gap_bps < 0
            && l.gap_bps.saturating_neg() >= cfg.min_withdraw_gap_bps
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
                    cfg.min_withdraw_gap_bps,
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
            cfg.min_withdraw_gap_bps,
        ));
    }

    // Step 4: idle cash present and above min-action.
    if idle_usd >= cfg.min_action_usd {
        let cap = cap_to_aum_fraction(total_aum_usd, cfg.max_action_fraction);
        let amount = idle_usd.min(cap);
        if amount >= cfg.min_action_usd {
            // Best DEPLOYABLE leveraged strategy = largest positive gap
            // to hurdle, excluding strategies the allocator cannot size
            // in USD. Without the deployable filter the picker can return
            // `multiply`, the envelope construction returns None, and
            // the orchestrator emits `skipped:no_dispatch` — leaving
            // idle un-deployed for the tick.
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
            // This is the right behaviour even when stable_yield's own
            // APR is the cause of the high hurdle: depositing into the
            // hurdle anchor is always at-worst neutral.
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
        // Idle present but max_action_fraction shrinks it below min.
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

    // Step 5: nothing to do.
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
                // idle 175.84 capped by max_action_fraction 0.5 * 239.37 = 119.69
                assert!(
                    (amount_usd - 119.685).abs() < 0.01,
                    "expected ~119.69, got {amount_usd}"
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
}
