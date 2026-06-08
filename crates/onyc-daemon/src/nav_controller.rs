//! NAV-aware LTV controller.
//!
//! The "above the protocol" element of onyc-daemon. Standard LTV
//! controllers (multiply's continuous-price clamp, vanilla Kamino
//! risk monitors) assume the collateral price moves continuously and
//! react to every tick. ONyc NAV doesn't move continuously — it
//! updates in discrete chunks via Chainlink Data Streams when the
//! monthly Apex Group attestation lands, and within a chunk when a
//! reinsurance claim event settles.
//!
//! A naive controller treats a single chunky NAV update as an
//! "emergency LTV drift" and triggers an auto-unwind that:
//!   1. realizes the NAV drop instead of waiting for it to mean-revert
//!   2. pays Orca slippage to exit through a $15M-depth secondary market
//!   3. surrenders the leverage position right when the regime was
//!      otherwise healthy
//!
//! The NAV-aware controller distinguishes:
//!   * **Discrete event** — a single NAV step within the buffer; expected
//!     for OnRe's settlement cadence; do NOT trigger auto-unwind.
//!   * **Sustained drift** — multiple consecutive NAV steps downward,
//!     or a single step beyond the absolute threshold; this IS a real
//!     impairment and warrants action.
//!
//! API: `decide_nav_response(history, threshold_bps_per_step,
//! max_consecutive_steps_down)` returns a `NavResponse` enum the
//! `liq_monitor` consumes when gating its auto-unwind decisions.
//!
//! Pure module — no RPC, no time, no I/O. Caller supplies the history
//! (wall-clock-stamped observations) and the threshold parameters.

use std::collections::VecDeque;

/// Maximum NAV observations kept in the rolling window. At one tick per
/// minute, 60 covers an hour — enough to capture the discrete-event
/// recovery window without growing unbounded.
pub const HISTORY_WINDOW_CAPACITY: usize = 60;

/// Default per-step buffer (bps). A single NAV step within ±50bps of
/// the prior reading is "noise / normal accrual" and not flagged as a
/// step-change event. Calibrated against OnRe's published NAV history:
/// monthly attestation deltas have averaged ~30-80bps in 2026.
pub const DEFAULT_STEP_BUFFER_BPS: i32 = 50;

/// Default absolute single-step threshold (bps). A NAV move greater than
/// this in a single step IS treated as a real impairment regardless of
/// the rolling pattern. 500bps = 5% — bigger than any clean attestation
/// has been; a 5% NAV drop signals an actual reinsurance loss event.
pub const DEFAULT_SINGLE_STEP_ALARM_BPS: i32 = 500;

/// Default streak threshold. After N consecutive downward steps,
/// even if each is within the buffer, dampening lifts and alerts pass
/// through. 3 steps of 50bps each = 150bps cumulative — large enough
/// to indicate sustained drift, not a transient single event.
pub const DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS: u32 = 3;

/// Single NAV observation (timestamp + value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavObservation {
    /// Wall-clock unix seconds when this observation was taken.
    pub ts_unix: u64,
    /// NAV value in micro-USD per whole token (e.g. $1.11 = 1_110_000).
    /// Using integer micro-USD avoids floating-point comparison noise.
    pub nav_micro_usd: u128,
}

/// Bounded ring buffer of NAV observations. New entries push to the
/// back; oldest drop from the front when capacity is hit.
#[derive(Debug, Default, Clone)]
pub struct NavHistory {
    obs: VecDeque<NavObservation>,
}

impl NavHistory {
    pub fn new() -> Self {
        Self {
            obs: VecDeque::with_capacity(HISTORY_WINDOW_CAPACITY),
        }
    }

    /// Record a new observation. Drops the oldest if at capacity.
    /// Idempotent on identical (ts, nav) — duplicates aren't pushed.
    pub fn observe(&mut self, ts_unix: u64, nav_micro_usd: u128) {
        if let Some(last) = self.obs.back() {
            if last.ts_unix == ts_unix && last.nav_micro_usd == nav_micro_usd {
                return;
            }
        }
        if self.obs.len() >= HISTORY_WINDOW_CAPACITY {
            self.obs.pop_front();
        }
        self.obs.push_back(NavObservation { ts_unix, nav_micro_usd });
    }

    pub fn len(&self) -> usize {
        self.obs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.obs.is_empty()
    }

    pub fn latest(&self) -> Option<&NavObservation> {
        self.obs.back()
    }

    pub fn prior(&self) -> Option<&NavObservation> {
        if self.obs.len() < 2 {
            return None;
        }
        self.obs.get(self.obs.len() - 2)
    }

    /// Count consecutive downward steps from the most recent observation
    /// backwards. Returns 0 if the latest observation is up or flat
    /// vs. the prior. Used to detect sustained drift.
    pub fn consecutive_down_streak(&self) -> u32 {
        if self.obs.len() < 2 {
            return 0;
        }
        let mut streak: u32 = 0;
        for w in self.obs.iter().rev().collect::<Vec<_>>().windows(2) {
            let latest = w[0];
            let prior = w[1];
            if latest.nav_micro_usd < prior.nav_micro_usd {
                streak += 1;
            } else {
                break;
            }
        }
        streak
    }
}

/// Pure helper: convert a Kamino obligation deposit slot
/// (deposited_amount in lamports, market_value_sf in 2^60-scaled USD)
/// into a per-whole-token NAV expressed in micro-USD (1e-6).
///
/// Math:
///   per_lamport_sf = market_value_sf / deposited_amount    [sf-USD per lamport]
///   per_whole_sf   = per_lamport_sf × 10^decimals          [sf-USD per token]
///   per_whole_usd  = per_whole_sf / 2^60                   [USD per token]
///   per_whole_micro_usd = per_whole_usd × 10^6             [micro-USD per token]
///
/// Returns `None` if `deposited_amount == 0` (divide-by-zero guard).
/// Returns `Some(0)` if market_value_sf == 0 with non-zero amount (edge
/// case — won't happen in practice but exhaustive).
pub fn nav_micro_usd_from_deposit(
    deposited_amount: u64,
    market_value_sf: u128,
    decimals: u8,
) -> Option<u128> {
    if deposited_amount == 0 {
        return None;
    }
    // Scale: (mv_sf × 10^decimals × 10^6) / (amount × 2^60)
    let scale_up = 10u128.pow(decimals as u32).saturating_mul(1_000_000);
    let numerator = market_value_sf.checked_mul(scale_up)?;
    let denominator = (deposited_amount as u128).checked_mul(1u128 << 60)?;
    if denominator == 0 {
        return None;
    }
    Some(numerator / denominator)
}

/// Per-step delta in bps. Positive = NAV up, negative = NAV down.
/// Computed against the prior observation in the history.
pub fn step_delta_bps(history: &NavHistory) -> Option<i32> {
    let latest = history.latest()?;
    let prior = history.prior()?;
    if prior.nav_micro_usd == 0 {
        return None;
    }
    let delta_micro: i128 =
        latest.nav_micro_usd as i128 - prior.nav_micro_usd as i128;
    let bps = delta_micro
        .saturating_mul(10_000)
        .checked_div(prior.nav_micro_usd as i128)?;
    Some(bps as i32)
}

/// The controller's decision on how to treat the most-recent NAV step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavResponse {
    /// Within the buffer or no prior observation; ignore for alerting
    /// purposes. Continue normal LTV monitoring without dampening.
    Normal,
    /// Single discrete NAV step within the buffer but downward. Dampen
    /// auto-unwind alerts — wait to see if it's a transient single
    /// event or the start of a sustained drift.
    DampenAlerts {
        step_delta_bps: i32,
        streak: u32,
    },
    /// Sustained downward drift (streak ≥ max_consecutive_down_steps)
    /// OR a single step exceeding the absolute alarm threshold.
    /// Auto-unwind alerts pass through normally.
    EscalateAlerts {
        step_delta_bps: i32,
        streak: u32,
        reason: EscalateReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalateReason {
    /// Single step exceeded the absolute single-step alarm threshold.
    SingleStepBeyondThreshold,
    /// Streak of consecutive downward steps reached the configured cap.
    SustainedDownwardDrift,
}

/// Decide the NAV response based on the rolling history and tuned
/// thresholds. Pure function — easy to test, easy to backtest.
pub fn decide_nav_response(
    history: &NavHistory,
    step_buffer_bps: i32,
    single_step_alarm_bps: i32,
    max_consecutive_down_steps: u32,
) -> NavResponse {
    let Some(delta_bps) = step_delta_bps(history) else {
        return NavResponse::Normal;
    };
    let streak = history.consecutive_down_streak();

    // Absolute single-step threshold trumps the streak count — a single
    // big move is itself the alarm.
    if delta_bps.abs() >= single_step_alarm_bps {
        return NavResponse::EscalateAlerts {
            step_delta_bps: delta_bps,
            streak,
            reason: EscalateReason::SingleStepBeyondThreshold,
        };
    }

    // Sustained-drift detection. Streak of N consecutive downward steps
    // means we've drifted past the noise floor.
    if streak >= max_consecutive_down_steps {
        return NavResponse::EscalateAlerts {
            step_delta_bps: delta_bps,
            streak,
            reason: EscalateReason::SustainedDownwardDrift,
        };
    }

    // Within the buffer + no streak alarm. If the step was downward,
    // dampen alerts so a single chunky update doesn't trigger an
    // unwind. If the step was up or zero, no dampening needed.
    if delta_bps < -step_buffer_bps {
        return NavResponse::DampenAlerts {
            step_delta_bps: delta_bps,
            streak,
        };
    }

    NavResponse::Normal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nav(micro: u128) -> u128 {
        micro
    }

    fn hist() -> NavHistory {
        NavHistory::new()
    }

    #[test]
    fn empty_history_is_normal() {
        let h = hist();
        assert_eq!(
            decide_nav_response(
                &h,
                DEFAULT_STEP_BUFFER_BPS,
                DEFAULT_SINGLE_STEP_ALARM_BPS,
                DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS
            ),
            NavResponse::Normal
        );
    }

    #[test]
    fn single_observation_is_normal() {
        let mut h = hist();
        h.observe(1_000, nav(1_110_000));
        assert_eq!(
            decide_nav_response(
                &h,
                DEFAULT_STEP_BUFFER_BPS,
                DEFAULT_SINGLE_STEP_ALARM_BPS,
                DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS
            ),
            NavResponse::Normal
        );
    }

    #[test]
    fn small_upward_step_is_normal() {
        let mut h = hist();
        h.observe(1_000, nav(1_110_000));
        h.observe(2_000, nav(1_115_000)); // +5bps gain
        assert_eq!(
            decide_nav_response(
                &h,
                DEFAULT_STEP_BUFFER_BPS,
                DEFAULT_SINGLE_STEP_ALARM_BPS,
                DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS
            ),
            NavResponse::Normal
        );
    }

    #[test]
    fn small_downward_step_dampens_alerts() {
        let mut h = hist();
        h.observe(1_000, nav(1_110_000));
        // 1_110_000 → 1_100_000 = -10_000 micro = -90bps (-0.90%)
        h.observe(2_000, nav(1_100_000));
        match decide_nav_response(
            &h,
            DEFAULT_STEP_BUFFER_BPS,
            DEFAULT_SINGLE_STEP_ALARM_BPS,
            DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS,
        ) {
            NavResponse::DampenAlerts { step_delta_bps, streak } => {
                assert!(step_delta_bps < 0);
                assert_eq!(streak, 1);
            }
            other => panic!("expected DampenAlerts, got {other:?}"),
        }
    }

    #[test]
    fn tiny_downward_step_within_buffer_is_normal() {
        let mut h = hist();
        h.observe(1_000, nav(1_110_000));
        // 1_110_000 → 1_109_500 = -500 micro = -4.5bps (within 50bps buffer)
        h.observe(2_000, nav(1_109_500));
        assert_eq!(
            decide_nav_response(
                &h,
                DEFAULT_STEP_BUFFER_BPS,
                DEFAULT_SINGLE_STEP_ALARM_BPS,
                DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS
            ),
            NavResponse::Normal
        );
    }

    #[test]
    fn large_single_step_down_escalates() {
        let mut h = hist();
        h.observe(1_000, nav(1_110_000));
        // 1_110_000 → 1_050_000 = -60_000 micro = -540bps (-5.4%)
        h.observe(2_000, nav(1_050_000));
        match decide_nav_response(
            &h,
            DEFAULT_STEP_BUFFER_BPS,
            DEFAULT_SINGLE_STEP_ALARM_BPS,
            DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS,
        ) {
            NavResponse::EscalateAlerts { reason, .. } => {
                assert_eq!(reason, EscalateReason::SingleStepBeyondThreshold);
            }
            other => panic!("expected EscalateAlerts(SingleStep), got {other:?}"),
        }
    }

    #[test]
    fn sustained_drift_escalates_after_streak() {
        let mut h = hist();
        // 3 consecutive ~80bps downward steps — none individually large
        // enough to trigger the single-step alarm (500bps), but the
        // streak hits the max_consecutive cap.
        h.observe(1_000, nav(1_110_000));
        h.observe(2_000, nav(1_101_000)); // -81bps
        h.observe(3_000, nav(1_092_000)); // -82bps
        h.observe(4_000, nav(1_083_000)); // -82bps
        match decide_nav_response(
            &h,
            DEFAULT_STEP_BUFFER_BPS,
            DEFAULT_SINGLE_STEP_ALARM_BPS,
            DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS,
        ) {
            NavResponse::EscalateAlerts { reason, streak, .. } => {
                assert_eq!(reason, EscalateReason::SustainedDownwardDrift);
                assert!(streak >= DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS);
            }
            other => panic!("expected EscalateAlerts(Drift), got {other:?}"),
        }
    }

    #[test]
    fn upward_step_breaks_streak() {
        let mut h = hist();
        h.observe(1_000, nav(1_110_000));
        h.observe(2_000, nav(1_100_000)); // -90bps
        h.observe(3_000, nav(1_090_000)); // -91bps
        h.observe(4_000, nav(1_120_000)); // +275bps (up)
        h.observe(5_000, nav(1_115_000)); // -45bps (within buffer)
        // After the upward step at t=4_000, the streak should reset.
        // The latest step at t=5_000 is within the 50bps buffer, so Normal.
        assert_eq!(
            decide_nav_response(
                &h,
                DEFAULT_STEP_BUFFER_BPS,
                DEFAULT_SINGLE_STEP_ALARM_BPS,
                DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS
            ),
            NavResponse::Normal
        );
    }

    #[test]
    fn streak_counter_resets_after_up_move() {
        let mut h = hist();
        h.observe(1_000, nav(1_110_000));
        h.observe(2_000, nav(1_100_000));
        h.observe(3_000, nav(1_090_000));
        h.observe(4_000, nav(1_095_000)); // up
        assert_eq!(h.consecutive_down_streak(), 0);
    }

    #[test]
    fn history_capacity_caps_growth() {
        let mut h = hist();
        for i in 0..(HISTORY_WINDOW_CAPACITY + 10) {
            h.observe(i as u64, nav(1_000_000 + i as u128));
        }
        assert_eq!(h.len(), HISTORY_WINDOW_CAPACITY);
    }

    #[test]
    fn duplicate_same_obs_is_ignored() {
        let mut h = hist();
        h.observe(1_000, nav(1_110_000));
        h.observe(1_000, nav(1_110_000));
        h.observe(1_000, nav(1_110_000));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn step_delta_bps_returns_none_for_short_history() {
        let mut h = hist();
        assert!(step_delta_bps(&h).is_none());
        h.observe(1_000, nav(1_110_000));
        assert!(step_delta_bps(&h).is_none());
    }

    #[test]
    fn step_delta_bps_computes_correctly() {
        let mut h = hist();
        h.observe(1_000, nav(1_000_000));
        h.observe(2_000, nav(990_000));
        // (990_000 - 1_000_000) * 10_000 / 1_000_000 = -100 bps
        assert_eq!(step_delta_bps(&h), Some(-100));
    }

    #[test]
    fn default_constants_calibrated_correctly() {
        // Pin the defaults — operator tunings rely on these.
        assert_eq!(DEFAULT_STEP_BUFFER_BPS, 50);
        assert_eq!(DEFAULT_SINGLE_STEP_ALARM_BPS, 500);
        assert_eq!(DEFAULT_MAX_CONSECUTIVE_DOWN_STEPS, 3);
        // Ratio sanity: single-step alarm should be at least 5× the
        // buffer (otherwise noise can trip the alarm).
        assert!(DEFAULT_SINGLE_STEP_ALARM_BPS >= 5 * DEFAULT_STEP_BUFFER_BPS);
    }
}
