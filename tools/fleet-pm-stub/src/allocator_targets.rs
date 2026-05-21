//! Target weight definitions for the allocator's drift-from-target mode.
//!
//! The allocator's rc21-and-earlier behaviour was greedy: each tick picked
//! the strategy with the best gap to its hurdle and emitted a single
//! Deposit/Withdraw against it. That produces an *emergent* allocation
//! (highest-APR-above-hurdle wins more deposits over time + cooldowns)
//! but no *designed* one. Operators couldn't say "I want 30% in
//! stable_yield, 30% in multiply, 40% in hedgedjlp" and have the
//! orchestrator drive toward those targets.
//!
//! [`TargetWeights`] is the operator-set tilt that converts the allocator
//! from "greedy best gap" into "close the largest drift from target."
//! Hurdles still gate Withdraw (the rc15 lesson), and the deployable
//! filter still excludes strategies the allocator can't size in USD —
//! target weights compose with those, they don't replace them.
//!
//! This module is intentionally inert for the M1 milestone of the
//! allocator v2 scope: it defines the struct, CLI parser, and lookup
//! helper, with no callers yet. Milestone 2 wires `decide()` to consult
//! `TargetWeights` when present.

use std::fmt;

/// Per-strategy target allocation weights. Each field is a fraction in
/// `[0.0, 1.0]`; the three should sum to 1.0 (the constructor accepts
/// a [0.99, 1.01] band and normalises to exactly 1.0).
///
/// A weight of 0.0 means "the allocator should hold no capital here."
/// Combined with the hurdle check this still allows the rc15-style
/// "carry-inverted → withdraw" path to fire on a strategy with target
/// 0.0 that nonetheless has live capital from a prior assignment.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TargetWeights {
    pub stable_yield: f64,
    pub multiply: f64,
    pub hedgedjlp: f64,
}

/// Errors produced when an operator-supplied set of target weights is
/// inconsistent. Surfaced from both [`TargetWeights::new`] (used by tests
/// and direct API callers) and [`TargetWeights::parse_cli`] (used by the
/// orchestrator's `--target-weights` flag).
#[derive(Debug, Clone, PartialEq)]
pub enum TargetWeightsError {
    /// A weight was below zero. Negative target = "the allocator should
    /// short this strategy", which the current design doesn't model.
    NegativeWeight { strategy: String, value: f64 },
    /// Pre-normalisation sum is outside the `[0.99, 1.01]` tolerance. A
    /// sum of 0.5 or 1.5 is almost certainly a typo, not a deliberate
    /// asymmetric allocation. We refuse rather than silently scale.
    SumOutOfRange { sum: f64 },
    /// CLI input referenced a strategy name we don't recognise. Helps
    /// operators catch typos like `stableyield=0.5` (missing underscore).
    UnknownStrategy { name: String },
    /// CLI input couldn't be split into `name=value` pairs.
    InvalidFormat { input: String },
    /// `value` portion of a CLI entry wasn't a finite f64.
    InvalidNumber { input: String, value: String },
}

impl fmt::Display for TargetWeightsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NegativeWeight { strategy, value } => {
                write!(
                    f,
                    "target weight for {strategy} is negative ({value}); the \
                     allocator cannot short — pass 0.0 instead"
                )
            }
            Self::SumOutOfRange { sum } => write!(
                f,
                "target weights sum to {sum:.4}; must be within [0.99, 1.01] \
                 (pre-normalisation tolerance — catches typos like 1.5)"
            ),
            Self::UnknownStrategy { name } => write!(
                f,
                "unknown strategy name '{name}' in target-weights input; \
                 expected one of: stable_yield, multiply, hedgedjlp"
            ),
            Self::InvalidFormat { input } => write!(
                f,
                "could not parse target-weights entry '{input}' as name=value"
            ),
            Self::InvalidNumber { input, value } => write!(
                f,
                "target-weights entry '{input}' has non-finite value '{value}'"
            ),
        }
    }
}

impl std::error::Error for TargetWeightsError {}

/// Tolerance band for the pre-normalisation sum check. Operators who hit
/// "0.30 + 0.30 + 0.40 = 1.0000001" because of f64 arithmetic should
/// pass; operators who type 0.30 + 0.30 + 0.50 (= 1.10) should fail.
pub const SUM_TOLERANCE_LO: f64 = 0.99;
pub const SUM_TOLERANCE_HI: f64 = 1.01;

impl TargetWeights {
    /// Construct from explicit field values, validating no-negative and
    /// sum-in-tolerance, then normalising so the final sum is exactly 1.0.
    ///
    /// Use this directly in tests or when wiring from a non-CLI source.
    /// For operator-facing CLI parsing, use [`parse_cli`](Self::parse_cli)
    /// which provides better error messages for malformed input.
    pub fn new(
        stable_yield: f64,
        multiply: f64,
        hedgedjlp: f64,
    ) -> Result<Self, TargetWeightsError> {
        for (name, w) in [
            ("stable_yield", stable_yield),
            ("multiply", multiply),
            ("hedgedjlp", hedgedjlp),
        ] {
            if !w.is_finite() {
                return Err(TargetWeightsError::InvalidNumber {
                    input: format!("{name}={w}"),
                    value: format!("{w}"),
                });
            }
            if w < 0.0 {
                return Err(TargetWeightsError::NegativeWeight {
                    strategy: name.to_string(),
                    value: w,
                });
            }
        }
        let sum = stable_yield + multiply + hedgedjlp;
        if !(SUM_TOLERANCE_LO..=SUM_TOLERANCE_HI).contains(&sum) {
            return Err(TargetWeightsError::SumOutOfRange { sum });
        }
        // Normalise so the final sum is exactly 1.0. This lets the rest
        // of the codebase treat the weights as a true probability
        // distribution without re-checking the tolerance band.
        let inv = if sum > 0.0 { 1.0 / sum } else { 0.0 };
        Ok(Self {
            stable_yield: stable_yield * inv,
            multiply: multiply * inv,
            hedgedjlp: hedgedjlp * inv,
        })
    }

    /// Parse an operator-facing CLI string of the shape
    /// `"stable_yield=0.30,multiply=0.30,hedgedjlp=0.40"`. Missing
    /// strategies default to 0.0; whitespace around delimiters is
    /// tolerated; the order of entries is irrelevant. Same validation
    /// + normalisation as [`new`](Self::new).
    pub fn parse_cli(s: &str) -> Result<Self, TargetWeightsError> {
        let mut stable_yield = 0.0;
        let mut multiply = 0.0;
        let mut hedgedjlp = 0.0;
        for raw in s.split(',') {
            let entry = raw.trim();
            if entry.is_empty() {
                continue; // Tolerate trailing commas and empty input.
            }
            let (name, value) = entry.split_once('=').ok_or_else(|| {
                TargetWeightsError::InvalidFormat {
                    input: entry.to_string(),
                }
            })?;
            let name = name.trim();
            let value_str = value.trim();
            let value: f64 =
                value_str
                    .parse()
                    .map_err(|_| TargetWeightsError::InvalidNumber {
                        input: entry.to_string(),
                        value: value_str.to_string(),
                    })?;
            match name {
                "stable_yield" => stable_yield = value,
                "multiply" => multiply = value,
                "hedgedjlp" => hedgedjlp = value,
                other => {
                    return Err(TargetWeightsError::UnknownStrategy {
                        name: other.to_string(),
                    })
                }
            }
        }
        Self::new(stable_yield, multiply, hedgedjlp)
    }

    /// Look up the target weight for a strategy by id. Returns 0.0 for
    /// unknown ids — the deposit picker uses this to mean "this strategy
    /// is not in the target allocation, so it should never receive
    /// allocator-driven deposits."
    ///
    /// This is the only public lookup surface; downstream code should
    /// not match on strategy strings directly so adding a new strategy
    /// in the future requires changing one place (this match), not
    /// every call-site.
    pub fn for_strategy(&self, id: &str) -> f64 {
        match id {
            "stable_yield" => self.stable_yield,
            "multiply" => self.multiply,
            "hedgedjlp" => self.hedgedjlp,
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn new_accepts_canonical_three_strategy_tilt() {
        let w = TargetWeights::new(0.30, 0.30, 0.40).expect("valid tilt");
        assert!(close(w.stable_yield, 0.30));
        assert!(close(w.multiply, 0.30));
        assert!(close(w.hedgedjlp, 0.40));
        // Sum is exactly 1.0 after normalisation.
        assert!(close(w.stable_yield + w.multiply + w.hedgedjlp, 1.0));
    }

    #[test]
    fn new_normalises_within_tolerance_band() {
        // 0.30 + 0.30 + 0.41 = 1.01 — at the boundary, must be accepted
        // and normalised to exactly 1.0.
        let w = TargetWeights::new(0.30, 0.30, 0.41).expect("at upper tol");
        let sum = w.stable_yield + w.multiply + w.hedgedjlp;
        assert!(
            close(sum, 1.0),
            "post-normalisation sum should be exactly 1.0, got {sum}"
        );
        // Ratio preserved: hedgedjlp should still be ~0.406 (0.41/1.01).
        assert!((w.hedgedjlp - 0.41 / 1.01).abs() < 1e-9);
    }

    #[test]
    fn new_rejects_sum_well_outside_tolerance() {
        // 0.5+0.5+0.5 = 1.5 — almost certainly a typo, not a deliberate
        // 50% triple-allocation. Refuse rather than scale.
        let err = TargetWeights::new(0.5, 0.5, 0.5).expect_err("sum 1.5");
        assert!(matches!(err, TargetWeightsError::SumOutOfRange { .. }));

        let err = TargetWeights::new(0.1, 0.1, 0.1).expect_err("sum 0.3");
        assert!(matches!(err, TargetWeightsError::SumOutOfRange { .. }));
    }

    #[test]
    fn new_rejects_negative_weight() {
        let err = TargetWeights::new(-0.1, 0.6, 0.5).expect_err("negative");
        match err {
            TargetWeightsError::NegativeWeight { strategy, .. } => {
                assert_eq!(strategy, "stable_yield");
            }
            other => panic!("expected NegativeWeight, got {other:?}"),
        }
    }

    #[test]
    fn new_rejects_non_finite() {
        assert!(matches!(
            TargetWeights::new(f64::NAN, 0.5, 0.5),
            Err(TargetWeightsError::InvalidNumber { .. })
        ));
        assert!(matches!(
            TargetWeights::new(f64::INFINITY, 0.5, 0.5),
            Err(TargetWeightsError::InvalidNumber { .. })
        ));
    }

    #[test]
    fn parse_cli_canonical() {
        let w = TargetWeights::parse_cli("stable_yield=0.30,multiply=0.30,hedgedjlp=0.40")
            .expect("parse canonical");
        assert!(close(w.stable_yield, 0.30));
        assert!(close(w.multiply, 0.30));
        assert!(close(w.hedgedjlp, 0.40));
    }

    #[test]
    fn parse_cli_missing_strategy_defaults_to_zero() {
        // Only stable_yield set — other two default to 0.0. Sum = 1.0
        // → valid. The whole AUM goes to stable_yield.
        let w = TargetWeights::parse_cli("stable_yield=1.0").expect("single");
        assert!(close(w.stable_yield, 1.0));
        assert!(close(w.multiply, 0.0));
        assert!(close(w.hedgedjlp, 0.0));
    }

    #[test]
    fn parse_cli_tolerates_whitespace_and_order() {
        let w = TargetWeights::parse_cli(
            " hedgedjlp = 0.40 , stable_yield=0.30 , multiply=0.30 ,",
        )
        .expect("messy");
        assert!(close(w.stable_yield, 0.30));
        assert!(close(w.hedgedjlp, 0.40));
    }

    #[test]
    fn parse_cli_rejects_unknown_strategy() {
        let err = TargetWeights::parse_cli("stableyield=1.0").expect_err("typo");
        match err {
            TargetWeightsError::UnknownStrategy { name } => {
                assert_eq!(name, "stableyield");
            }
            other => panic!("expected UnknownStrategy, got {other:?}"),
        }
    }

    #[test]
    fn parse_cli_rejects_malformed_entry() {
        assert!(matches!(
            TargetWeights::parse_cli("stable_yield"),
            Err(TargetWeightsError::InvalidFormat { .. })
        ));
        assert!(matches!(
            TargetWeights::parse_cli("stable_yield=abc"),
            Err(TargetWeightsError::InvalidNumber { .. })
        ));
    }

    #[test]
    fn for_strategy_lookup() {
        let w = TargetWeights::new(0.30, 0.30, 0.40).unwrap();
        assert!(close(w.for_strategy("stable_yield"), 0.30));
        assert!(close(w.for_strategy("multiply"), 0.30));
        assert!(close(w.for_strategy("hedgedjlp"), 0.40));
        // Unknown strategy → 0.0 (the "not in the target allocation" sentinel).
        assert_eq!(w.for_strategy("unknown_strategy"), 0.0);
        assert_eq!(w.for_strategy(""), 0.0);
    }

    #[test]
    fn error_display_messages_mention_actionable_remediation() {
        // Display strings are what the operator sees when their --target-
        // weights flag is wrong. Pin that they contain enough context to
        // act on (not just "invalid input").
        let err = TargetWeights::new(-0.5, 0.5, 1.0).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("stable_yield"), "should name the strategy: {msg}");
        assert!(msg.contains("0.0"), "should suggest 0.0 as the fix: {msg}");

        let err = TargetWeights::new(0.5, 0.5, 0.5).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("1.5") || msg.contains("typo"), "should explain: {msg}");

        let err = TargetWeights::parse_cli("nope=0.5").unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("stable_yield"),
            "should list the expected strategy names: {msg}"
        );
    }

    #[test]
    fn empty_cli_input_returns_all_zeros_which_is_invalid() {
        // Empty CLI string → all weights default to 0.0 → sum = 0.0 →
        // out of tolerance → error. Operators using `--target-weights=""`
        // get a clear failure rather than the allocator silently doing
        // nothing.
        let err = TargetWeights::parse_cli("").unwrap_err();
        assert!(matches!(err, TargetWeightsError::SumOutOfRange { .. }));
    }
}
