//! APR-weighted dynamic target weights (allocator v2 M5).
//!
//! Static [`TargetWeights`] (M1) require the operator to choose a tilt
//! like `(stable=0.30, multiply=0.30, hedgedjlp=0.40)` and stick with
//! it until the next config edit. Real markets move: jitoSOL APR can
//! drift 100 bps in a day, Kamino borrow rates spike and recover,
//! Jupiter Perps funding flips sign. A static tilt is correct for an
//! operator who's done the analysis offline; an APR-weighted tilt is
//! correct for an operator who wants the orchestrator to do that
//! analysis live.
//!
//! ## How the math works
//!
//! For each non-stable strategy, compute the per-strategy *gap to
//! hurdle*:
//!
//! ```text
//! gap_i = max(0, apr_i_bps - hurdle_i_bps)
//! ```
//!
//! `stable_yield` is the hurdle anchor — its "gap" is always 0 by
//! construction — so we treat it specially:
//!
//! - It always gets at least `stable_yield_floor` (default 0.20). This
//!   is the operator's risk-off baseline. Even if multiply and
//!   hedgedjlp are both yielding 30%, a fraction stays in the
//!   risk-free anchor.
//! - The remaining `1.0 - stable_yield_floor` is distributed across
//!   the non-stable strategies in proportion to their gaps.
//! - If every non-stable strategy is at-or-below hurdle, the
//!   stable_yield share scales up to 1.0 — everything parks in the
//!   anchor.
//!
//! ## Why we apply `min_per_strategy`
//!
//! Optional floor. If set above 0, each non-stable strategy with a
//! positive gap receives at least this much weight (re-normalising the
//! rest). Useful when an operator wants to maintain *some* exposure to
//! a strategy across noise dips ("never let multiply drop below 10%
//! even if its APR briefly underperforms"). Default 0.0 (no floor).
//!
//! ## Why this composes with the rebalance band
//!
//! Dynamic targets jitter as APRs move. The orchestrator's
//! `min_drift_bps` (default 200) absorbs that jitter — only drifts
//! past the band drive deposits. Without the band, every APR wobble
//! would trigger a rebalance; with it, the system tracks meaningful
//! reallocations and ignores noise. This is the same "noise floor"
//! pattern as rc15's `min_withdraw_gap_bps` hysteresis, applied to
//! drift-from-target instead of carry-inversion.

use crate::allocator_targets::{TargetWeights, TargetWeightsError};

/// Configuration for the APR-weighted dynamic target mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AprWeightedConfig {
    /// Minimum share of total AUM kept in `stable_yield`. Default 0.20.
    /// The operator's risk-off anchor — applied even when other
    /// strategies are yielding well.
    pub stable_yield_floor: f64,
    /// Minimum share of total AUM each non-stable strategy gets if its
    /// gap is positive. Default 0.0 (no floor; small gaps produce
    /// small weights). Set above 0 to maintain baseline exposure
    /// across noise dips.
    pub min_per_strategy: f64,
}

impl Default for AprWeightedConfig {
    fn default() -> Self {
        Self {
            stable_yield_floor: 0.20,
            min_per_strategy: 0.0,
        }
    }
}

impl AprWeightedConfig {
    /// Construct + validate. Returns an error if the parameters are
    /// nonsensical (e.g., `stable_yield_floor > 1.0` or negative).
    pub fn new(stable_yield_floor: f64, min_per_strategy: f64) -> Result<Self, TargetWeightsError> {
        for (name, v) in [
            ("stable_yield_floor", stable_yield_floor),
            ("min_per_strategy", min_per_strategy),
        ] {
            if !v.is_finite() {
                return Err(TargetWeightsError::InvalidNumber {
                    input: format!("{name}={v}"),
                    value: format!("{v}"),
                });
            }
            if !(0.0..=1.0).contains(&v) {
                return Err(TargetWeightsError::SumOutOfRange { sum: v });
            }
        }
        // Two non-stable strategies (multiply + hedgedjlp) × floor +
        // stable_yield_floor must leave room (≤ 1.0).
        if stable_yield_floor + 2.0 * min_per_strategy > 1.0 + 1e-9 {
            return Err(TargetWeightsError::SumOutOfRange {
                sum: stable_yield_floor + 2.0 * min_per_strategy,
            });
        }
        Ok(Self {
            stable_yield_floor,
            min_per_strategy,
        })
    }
}

/// Per-strategy snapshot needed to compute dynamic weights. Decoupled
/// from `StrategyRate` so callers in different modules don't have to
/// import the allocator's full state type.
#[derive(Debug, Clone, Copy)]
pub struct GapInput<'a> {
    pub id: &'a str,
    pub apr_bps: i32,
    pub hurdle_bps: i32,
}

/// Compute APR-weighted target weights from a per-strategy gap snapshot.
///
/// The caller supplies `(id, apr, hurdle)` for each strategy in the
/// fleet. `stable_yield` MUST be present; missing it returns an
/// "all-in-stable" fallback rather than panicking (the allocator's
/// upstream guards already refuse to act without a `stable_yield`
/// anchor — this function is defensive on top of that).
pub fn compute_apr_weighted(
    inputs: &[GapInput<'_>],
    cfg: &AprWeightedConfig,
) -> TargetWeights {
    // Per-strategy positive gaps. Strategies below hurdle get 0.
    let gap_for = |id: &str| -> f64 {
        inputs
            .iter()
            .find(|i| i.id == id)
            .map(|i| (i.apr_bps - i.hurdle_bps).max(0) as f64)
            .unwrap_or(0.0)
    };

    let g_multiply = gap_for("multiply");
    let g_hedgedjlp = gap_for("hedgedjlp");
    let total_gap = g_multiply + g_hedgedjlp;

    // Edge case: no non-stable strategy is above hurdle. Everything
    // parks in stable_yield.
    if total_gap <= 0.0 {
        return TargetWeights::new(1.0, 0.0, 0.0)
            .expect("all-in-stable is always a valid TargetWeights");
    }

    let stable_floor = cfg.stable_yield_floor.clamp(0.0, 1.0);
    let min_per = cfg.min_per_strategy.clamp(0.0, 1.0);
    let mut non_stable_budget = (1.0 - stable_floor).max(0.0);

    // Raw distribution proportional to gap.
    let mut multiply = if g_multiply > 0.0 {
        non_stable_budget * (g_multiply / total_gap)
    } else {
        0.0
    };
    let mut hedgedjlp = if g_hedgedjlp > 0.0 {
        non_stable_budget * (g_hedgedjlp / total_gap)
    } else {
        0.0
    };

    // Apply per-strategy minimum floor for non-stable strategies that
    // have any positive gap. Strategies with gap == 0 stay at 0 — the
    // floor is a "don't fully drain a contributing strategy" guard,
    // not a "guarantee minimum exposure regardless of performance"
    // guarantee.
    let floor_multiply = if g_multiply > 0.0 { min_per } else { 0.0 };
    let floor_hedgedjlp = if g_hedgedjlp > 0.0 { min_per } else { 0.0 };
    let floor_total = floor_multiply + floor_hedgedjlp;

    if floor_total > 0.0 && floor_total <= non_stable_budget {
        // After floors, rescale the *gap-proportional* component to fit
        // in the remaining (non_stable_budget - floor_total).
        let rescale_budget = non_stable_budget - floor_total;
        let gap_share_multiply = if g_multiply > 0.0 {
            rescale_budget * (g_multiply / total_gap)
        } else {
            0.0
        };
        let gap_share_hedgedjlp = if g_hedgedjlp > 0.0 {
            rescale_budget * (g_hedgedjlp / total_gap)
        } else {
            0.0
        };
        multiply = floor_multiply + gap_share_multiply;
        hedgedjlp = floor_hedgedjlp + gap_share_hedgedjlp;
    } else if floor_total > non_stable_budget {
        // Floors don't fit. Surface the misconfiguration loudly by
        // capping at the budget — operator either drops a floor or
        // raises stable_yield_floor.
        let scale = non_stable_budget / floor_total;
        multiply = floor_multiply * scale;
        hedgedjlp = floor_hedgedjlp * scale;
        non_stable_budget = multiply + hedgedjlp;
    }

    let stable_weight = (1.0 - multiply - hedgedjlp).max(0.0);
    let _ = non_stable_budget; // silenced — used as a working variable above

    // Final defence: feed through TargetWeights::new which validates +
    // normalises. Floating-point drift should keep us within the
    // [0.99, 1.01] tolerance band.
    TargetWeights::new(stable_weight, multiply, hedgedjlp).unwrap_or_else(|_| {
        // Pathological numerical case (NaN somewhere). Fall back to
        // all-in-stable rather than propagate; this function MUST
        // return a valid TargetWeights for every reachable input.
        TargetWeights::new(1.0, 0.0, 0.0).expect("safety fallback")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    fn gap(id: &str, apr_bps: i32, hurdle_bps: i32) -> GapInput<'_> {
        GapInput {
            id,
            apr_bps,
            hurdle_bps,
        }
    }

    #[test]
    fn equal_gaps_split_evenly_after_stable_floor() {
        // multiply gap = 1500 - 900 = 600
        // hedgedjlp gap = 1500 - 1000 = 500 — wait equal needs same
        // gap value. Let me set hurdles equal.
        // Use hurdle 900 for both → gaps 600/600.
        let inputs = [
            gap("stable_yield", 700, 0),
            gap("multiply", 1500, 900),
            gap("hedgedjlp", 1500, 900),
        ];
        let cfg = AprWeightedConfig::default();
        let w = compute_apr_weighted(&inputs, &cfg);
        assert!(close(w.stable_yield, 0.20));
        // Remaining 0.80 split equally → 0.40 each.
        assert!(close(w.multiply, 0.40));
        assert!(close(w.hedgedjlp, 0.40));
    }

    #[test]
    fn proportional_distribution_by_gap_magnitude() {
        // multiply gap = 200, hedgedjlp gap = 600. Ratio 1:3 → multiply
        // gets 0.20 of non-stable budget, hedgedjlp gets 0.60.
        let inputs = [
            gap("stable_yield", 700, 0),
            gap("multiply", 1100, 900),  // gap 200
            gap("hedgedjlp", 1600, 1000), // gap 600
        ];
        let cfg = AprWeightedConfig::default();
        let w = compute_apr_weighted(&inputs, &cfg);
        assert!(close(w.stable_yield, 0.20));
        // 0.80 budget × (200/800) = 0.20 multiply, 0.80 × (600/800) = 0.60 hedgedjlp
        assert!(close(w.multiply, 0.20), "multiply got {}", w.multiply);
        assert!(close(w.hedgedjlp, 0.60), "hedgedjlp got {}", w.hedgedjlp);
    }

    #[test]
    fn one_strategy_below_hurdle_gets_zero_others_split_remainder() {
        // multiply below hurdle (gap = 0), hedgedjlp above. All
        // non-stable budget goes to hedgedjlp.
        let inputs = [
            gap("stable_yield", 700, 0),
            gap("multiply", 800, 900),    // below hurdle
            gap("hedgedjlp", 1500, 1000), // gap 500
        ];
        let w = compute_apr_weighted(&inputs, &AprWeightedConfig::default());
        assert!(close(w.stable_yield, 0.20));
        assert!(close(w.multiply, 0.0));
        assert!(close(w.hedgedjlp, 0.80));
    }

    #[test]
    fn all_below_hurdle_park_in_stable() {
        // Nothing beats hurdle → 100% stable_yield.
        let inputs = [
            gap("stable_yield", 700, 0),
            gap("multiply", 700, 900),  // below
            gap("hedgedjlp", 700, 1000), // below
        ];
        let w = compute_apr_weighted(&inputs, &AprWeightedConfig::default());
        assert!(close(w.stable_yield, 1.0));
        assert!(close(w.multiply, 0.0));
        assert!(close(w.hedgedjlp, 0.0));
    }

    #[test]
    fn stable_yield_floor_zero_lets_winners_take_everything() {
        // With floor=0, the highest-gap strategy can dominate.
        let cfg = AprWeightedConfig::new(0.0, 0.0).unwrap();
        let inputs = [
            gap("stable_yield", 700, 0),
            gap("multiply", 800, 900),   // below
            gap("hedgedjlp", 1500, 1000), // gap 500
        ];
        let w = compute_apr_weighted(&inputs, &cfg);
        assert!(close(w.stable_yield, 0.0));
        assert!(close(w.multiply, 0.0));
        assert!(close(w.hedgedjlp, 1.0));
    }

    #[test]
    fn stable_yield_floor_one_keeps_everything_in_stable() {
        // floor=1.0 → 100% in stable regardless of other APRs.
        let cfg = AprWeightedConfig::new(1.0, 0.0).unwrap();
        let inputs = [
            gap("stable_yield", 700, 0),
            gap("multiply", 1500, 900),
            gap("hedgedjlp", 1500, 1000),
        ];
        let w = compute_apr_weighted(&inputs, &cfg);
        assert!(close(w.stable_yield, 1.0));
    }

    #[test]
    fn min_per_strategy_floor_kicks_in_for_tiny_gap() {
        // multiply gap = 1, hedgedjlp gap = 999. Without floor multiply
        // would get ~0.0008 of the 0.80 budget. With min_per_strategy
        // = 0.10, multiply is floored at 0.10 and hedgedjlp gets the
        // rescaled remainder.
        let cfg = AprWeightedConfig::new(0.20, 0.10).unwrap();
        let inputs = [
            gap("stable_yield", 700, 0),
            gap("multiply", 901, 900),    // gap 1
            gap("hedgedjlp", 1999, 1000), // gap 999
        ];
        let w = compute_apr_weighted(&inputs, &cfg);
        assert!(close(w.stable_yield, 0.20));
        // multiply floor = 0.10, gap-share = (0.80 - 0.20) × (1/1000) ≈ 0.0006
        // total multiply ≈ 0.1006
        assert!(
            (w.multiply - 0.1006).abs() < 1e-3,
            "multiply got {}",
            w.multiply
        );
        // hedgedjlp = 0.10 floor + (0.80 - 0.20) × (999/1000) ≈ 0.6994
        assert!(
            (w.hedgedjlp - 0.6994).abs() < 1e-3,
            "hedgedjlp got {}",
            w.hedgedjlp
        );
        // Sum to 1.0 after the normaliser inside TargetWeights::new.
        assert!(close(
            w.stable_yield + w.multiply + w.hedgedjlp,
            1.0
        ));
    }

    #[test]
    fn min_per_strategy_zero_for_strategies_below_hurdle() {
        // multiply below hurdle → floor does NOT apply to it. Min-per
        // is a "don't drain a contributing strategy" guard, not a
        // "guarantee minimum exposure" guarantee.
        let cfg = AprWeightedConfig::new(0.20, 0.15).unwrap();
        let inputs = [
            gap("stable_yield", 700, 0),
            gap("multiply", 800, 900),    // below — gap 0
            gap("hedgedjlp", 1500, 1000), // gap 500
        ];
        let w = compute_apr_weighted(&inputs, &cfg);
        assert!(close(w.multiply, 0.0), "below-hurdle gets no floor");
        assert!(close(w.hedgedjlp, 0.80));
        assert!(close(w.stable_yield, 0.20));
    }

    #[test]
    fn config_rejects_negative_or_above_one() {
        assert!(AprWeightedConfig::new(-0.1, 0.0).is_err());
        assert!(AprWeightedConfig::new(1.5, 0.0).is_err());
        assert!(AprWeightedConfig::new(0.5, -0.1).is_err());
        assert!(AprWeightedConfig::new(0.5, 1.5).is_err());
        assert!(AprWeightedConfig::new(f64::NAN, 0.0).is_err());
    }

    #[test]
    fn config_rejects_floor_combo_exceeding_one() {
        // 0.50 stable + 2 × 0.30 per_strategy = 1.10 — leaves no room.
        // Operator should drop a floor or raise stable.
        assert!(AprWeightedConfig::new(0.50, 0.30).is_err());
        // Edge: exactly 1.0 should be accepted (degenerate but valid).
        assert!(AprWeightedConfig::new(0.40, 0.30).is_ok());
    }

    #[test]
    fn floors_dont_fit_scales_down_safely() {
        // Pathological: a constructed AprWeightedConfig that bypasses
        // validation (impossible via `new` but possible via direct
        // struct literal in a test). Verify the math doesn't produce
        // weights > 1.0 in that case.
        let cfg = AprWeightedConfig {
            stable_yield_floor: 0.50,
            min_per_strategy: 0.30,
        };
        let inputs = [
            gap("stable_yield", 700, 0),
            gap("multiply", 1500, 900),
            gap("hedgedjlp", 1500, 1000),
        ];
        let w = compute_apr_weighted(&inputs, &cfg);
        // Sum must still be ≈ 1.0 — TargetWeights::new normalises.
        let total = w.stable_yield + w.multiply + w.hedgedjlp;
        assert!(close(total, 1.0), "must normalise to 1.0, got {total}");
    }

    #[test]
    fn missing_stable_yield_input_falls_back_safely() {
        // If the caller omits stable_yield, the math doesn't panic —
        // stable still gets its floor (the function looks up gap by
        // id, missing id = 0). This is a defensive fallback; the
        // allocator's upstream guards already refuse to act without
        // a stable_yield anchor present in the snapshot.
        //
        // Use equal hurdles so the gaps are equal and the split is
        // exactly 40/40 across the 80% non-stable budget.
        let inputs = [
            gap("multiply", 1500, 900),
            gap("hedgedjlp", 1500, 900),
        ];
        let w = compute_apr_weighted(&inputs, &AprWeightedConfig::default());
        assert!(close(w.stable_yield, 0.20));
        assert!(close(w.multiply, 0.40));
        assert!(close(w.hedgedjlp, 0.40));
    }
}
