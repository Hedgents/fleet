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
//! 3. If any leveraged strategy is deployed AND its APR is below hurdle,
//!    recommend `Withdraw` of its position (clamped to
//!    `max_action_fraction × total_aum_usd`). Pick the WORST under-hurdle
//!    strategy first (largest negative gap).
//! 4. Otherwise, if `idle_usd > min_action_usd`:
//!    - if at least one leveraged strategy is above hurdle, pick the one
//!      with the largest positive gap and `Deposit` idle to it;
//!    - else `Deposit` idle to `stable_yield`.
//! 5. Otherwise `NoAction`.

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
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self {
            risk_premium_bps_multiply: 200,
            risk_premium_bps_hedgedjlp: 300,
            min_action_usd: 5.0,
            max_action_fraction: 0.5,
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

    // Step 3: any DEPLOYED leveraged strategy under its hurdle → Withdraw
    // the worst offender (largest negative gap). This is the heart of the
    // regime-aware behaviour: when Kamino SOL borrow spikes and multiply
    // earns less than stable_yield + premium, unwind to stable_yield.
    levs.sort_by_key(|l| l.gap_bps); // ascending — worst (most negative) first
    if let Some(worst) = levs
        .iter()
        .find(|l| l.s.deployed_usd > 0.0 && l.gap_bps < 0)
    {
        let cap = cap_to_aum_fraction(total_aum_usd, cfg.max_action_fraction);
        let amount = worst.s.deployed_usd.min(cap);
        if amount < cfg.min_action_usd {
            return AllocatorAction::NoAction {
                reason: format!(
                    "{} under hurdle ({} < {} = {}+{}), but action ${:.2} below min ${:.2}",
                    worst.s.id,
                    fmt_bps(worst.s.nominal_apr_bps),
                    fmt_bps(worst.hurdle_bps),
                    fmt_bps(risk_free),
                    fmt_bps(worst.hurdle_bps - risk_free),
                    amount,
                    cfg.min_action_usd,
                ),
            };
        }
        return AllocatorAction::Withdraw {
            strategy: worst.s.id.clone(),
            amount_usd: amount,
            reason: format!(
                "carry inverted: {} earning {} < hurdle {} ({} + {} risk premium)",
                worst.s.id,
                fmt_bps(worst.s.nominal_apr_bps),
                fmt_bps(worst.hurdle_bps),
                fmt_bps(risk_free),
                fmt_bps(worst.hurdle_bps - risk_free),
            ),
        };
    }

    // Step 4: idle cash present and above min-action.
    if idle_usd >= cfg.min_action_usd {
        let cap = cap_to_aum_fraction(total_aum_usd, cfg.max_action_fraction);
        let amount = idle_usd.min(cap);
        if amount < cfg.min_action_usd {
            return AllocatorAction::NoAction {
                reason: format!(
                    "idle ${:.2} present but max_action_fraction caps action to ${:.2} \
                     (below min ${:.2})",
                    idle_usd, amount, cfg.min_action_usd,
                ),
            };
        }
        // Best leveraged strategy = largest positive gap to hurdle.
        levs.sort_by_key(|l| -l.gap_bps); // descending — best first
        if let Some(best) = levs.first() {
            if best.gap_bps > 0 {
                return AllocatorAction::Deposit {
                    strategy: best.s.id.clone(),
                    amount_usd: amount,
                    reason: format!(
                        "{} beats hurdle by {} ({} vs {} = {}+{} hurdle)",
                        best.s.id,
                        fmt_bps(best.gap_bps),
                        fmt_bps(best.s.nominal_apr_bps),
                        fmt_bps(best.hurdle_bps),
                        fmt_bps(risk_free),
                        fmt_bps(best.hurdle_bps - risk_free),
                    ),
                };
            }
        }
        // No leveraged strategy above hurdle → park in stable_yield.
        return AllocatorAction::Deposit {
            strategy: "stable_yield".to_string(),
            amount_usd: amount,
            reason: format!(
                "no leveraged strategy above hurdle; park idle ${:.2} in stable_yield @ {}",
                idle_usd,
                fmt_bps(risk_free),
            ),
        };
    }

    // Step 5: nothing to do.
    AllocatorAction::NoAction {
        reason: format!(
            "all deployed strategies meet hurdle; idle ${:.2} below min ${:.2}",
            idle_usd, cfg.min_action_usd,
        ),
    }
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
    fn multiply_just_below_hurdle_still_withdraws() {
        // stable 7% + 2% = 9% hurdle. Multiply at 8.99% → withdraw.
        let s = vec![
            sr("stable_yield", 500.0, 700),
            sr("multiply", 100.0, 899),
            sr("hedgedjlp", 0.0, 1500),
        ];
        match decide(&s, 600.0, 0.0, &cfg()) {
            AllocatorAction::Withdraw { strategy, .. } => assert_eq!(strategy, "multiply"),
            other => panic!("expected Withdraw, got {:?}", other),
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
    fn multiply_above_hurdle_with_idle_deposits_to_multiply() {
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
                assert_eq!(strategy, "multiply");
                // idle 50 < cap of 0.5 * 1050 = 525, so unconstrained.
                assert!((amount_usd - 50.0).abs() < 1e-9);
            }
            other => panic!("expected Deposit to multiply, got {:?}", other),
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
        // Deployed only $2, below min_action $5 → NoAction.
        let s = vec![sr("stable_yield", 100.0, 700), sr("multiply", 2.0, 100)];
        let c = AllocatorConfig {
            max_action_fraction: 1.0,
            ..AllocatorConfig::default()
        };
        match decide(&s, 102.0, 0.0, &c) {
            AllocatorAction::NoAction { reason } => {
                assert!(reason.contains("below min"));
            }
            other => panic!("expected NoAction, got {:?}", other),
        }
    }
}
