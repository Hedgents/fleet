//! Auto-mode promotion (M11): the daemon can optionally auto-accept
//! Assign/Withdraw envelopes from the trusted orchestrator, bypassing
//! the manual `Approve` queue, subject to bounded caps.
//!
//! Why: today every Assign/Withdraw envelope from the orchestrator
//! requires a manual `Approve` envelope from a human operator. For the
//! fleet's "autonomous regime-aware allocator" thesis to hold, strategy
//! daemons must auto-accept envelopes from a trusted orchestrator,
//! scoped only by the caps configured here.
//!
//! Defaults are conservative; operators must opt in explicitly per daemon
//! via `--auto-accept-orchestrator=true` (default false).
//!
//! Multiply-daemon owns two payload shapes:
//!   * AssignMultiply — sized by `target_ltv_bps` (no USD field).
//!     Auto-mode gate is `target_ltv_bps <= auto_max_target_ltv_bps`.
//!   * WithdrawMultiply — full-unwind only (no amount field). Treated as
//!     "always falls through" — the operator owns the unwind decision
//!     because the cumulative-USD cap can't bound a full-unwind in a
//!     useful way without per-position USD reads. The orchestrator can
//!     still reach the leverage executor for the common deleverage signal
//!     by sending an Assign with `target_ltv_bps=0` (full deleverage),
//!     which IS auto-accepted under the `--auto-allow-deleverage-always`
//!     escape hatch.
//!
//! 24h tracking is in-memory only (per the existing rebalancer-state
//! pattern). On daemon restart the window resets; caps still bound the
//! single-action and cooldown per-tick.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use zerox1_protocol::fleet::multiply::AssignMultiply;

/// CLI-configurable knobs that govern auto-mode. Bundled into the
/// `DispatchCtx` so the dispatch handler can consult them.
#[derive(Debug, Clone)]
pub struct AutoModeConfig {
    /// Master switch — false = every envelope queues (legacy behaviour).
    pub enabled: bool,
    /// Single-action cap, USD lamports (6 decimals). $50 default = 50e6.
    /// Above this → fall through to manual queue.
    pub max_single_action_usd_lamports: u64,
    /// 24h sliding-window cumulative cap, USD lamports. $200 default.
    pub max_cumulative_24h_usd_lamports: u64,
    /// Minimum seconds between two consecutive auto-accepts. Default 60.
    pub cooldown_secs: u64,
    /// Multiply-only: refuse auto-accept if requested target_ltv > this.
    /// Default 6500 (65%).
    pub max_target_ltv_bps: u16,
    /// Multiply-only: target_ltv_bps == 0 (full deleverage) always
    /// auto-accepts regardless of cumulative cap. Default true.
    pub allow_deleverage_always: bool,
}

impl Default for AutoModeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_single_action_usd_lamports: 50_000_000,        // $50
            max_cumulative_24h_usd_lamports: 200_000_000,      // $200
            cooldown_secs: 60,
            max_target_ltv_bps: 6500,
            allow_deleverage_always: true,
        }
    }
}

/// In-memory state for auto-mode caps. Shared via `Arc` across the
/// dispatch tasks; restart-volatile (acceptable — orchestrator re-emits
/// recommendations on its next tick, and the per-action caps still bound
/// individual envelopes).
#[derive(Debug, Default)]
pub struct AutoModeState {
    /// Recent auto-accept (unix_seconds, usd_lamports) tuples. Pruned to
    /// the trailing 24h on each check. VecDeque for cheap front-pop.
    window: Mutex<VecDeque<(u64, u64)>>,
    /// Wall-clock seconds of the last auto-accept. 0 = none yet.
    last_accept_unix: Mutex<u64>,
}

/// Trailing-window length for the cumulative cap.
pub const WINDOW_SECS: u64 = 24 * 60 * 60;

impl AutoModeState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sum of usd_lamports in the trailing 24h window, after pruning
    /// entries older than `now - WINDOW_SECS`.
    pub fn cumulative_24h_at(&self, now: u64) -> u64 {
        let mut w = self.window.lock().expect("auto_mode window poisoned");
        prune(&mut w, now);
        w.iter().map(|(_, u)| *u).sum()
    }

    /// Seconds since the most recent auto-accept. `now` is the current
    /// wall-clock; if no prior accept exists, returns `u64::MAX`.
    pub fn secs_since_last_accept_at(&self, now: u64) -> u64 {
        let l = *self
            .last_accept_unix
            .lock()
            .expect("auto_mode last_accept poisoned");
        if l == 0 {
            u64::MAX
        } else {
            now.saturating_sub(l)
        }
    }

    /// Record a successful auto-accept. Caller must have already
    /// satisfied the caps via `gate_*` before reaching here.
    pub fn record_at(&self, now: u64, usd_lamports: u64) {
        {
            let mut w = self.window.lock().expect("auto_mode window poisoned");
            prune(&mut w, now);
            w.push_back((now, usd_lamports));
        }
        let mut l = self
            .last_accept_unix
            .lock()
            .expect("auto_mode last_accept poisoned");
        *l = now;
    }
}

fn prune(w: &mut VecDeque<(u64, u64)>, now: u64) {
    let cutoff = now.saturating_sub(WINDOW_SECS);
    while let Some(&(t, _)) = w.front() {
        if t < cutoff {
            w.pop_front();
        } else {
            break;
        }
    }
}

/// Outcome of an auto-mode gate check.
#[derive(Debug, PartialEq, Eq)]
pub enum GateDecision {
    /// Auto-accept the envelope inline. Caller skips the queue and runs
    /// the execution function directly.
    Accept {
        /// USD-equivalent size of this action (lamports). Recorded into
        /// the 24h window once execution begins.
        usd_lamports: u64,
        /// Label for the auto-accept INFO log line.
        label: &'static str,
    },
    /// Reject auto-mode; fall through to the manual approval queue. The
    /// caller emits a WARN log including `cap` (which cap blew) +
    /// `reason` (human string).
    FallThrough {
        cap: &'static str,
        reason: String,
    },
}

/// Outer decision — combines the orchestrator sender-match check with
/// the cap gate. Used by the dispatcher to choose between "execute
/// inline now" vs "drop into the manual approval queue exactly as
/// before". Pure function for testability.
#[derive(Debug, PartialEq, Eq)]
pub enum DispatchPath {
    /// Bypass the manual approval queue and execute via the shared
    /// execute helper now. The caller is responsible for `record_at()`
    /// before invoking the executor so the 24h window reflects the
    /// pending tx.
    AutoExecute {
        usd_lamports: u64,
        label: &'static str,
    },
    /// Take the normal queue path. Cause is encoded in `cap` (one of
    /// "auto-mode-disabled", "non-orchestrator-sender", "target_ltv",
    /// "cooldown", "single_action_usd", "cumulative_24h"). `reason`
    /// is the human-readable diagnostic logged at WARN.
    Queue {
        cap: &'static str,
        reason: String,
    },
}

/// Decide the dispatch path for an AssignMultiply envelope. Public so
/// the dispatch handler can call it; tested as a pure function below.
pub fn decide_assign_multiply(
    cfg: &AutoModeConfig,
    state: &AutoModeState,
    orchestrator: Option<[u8; 32]>,
    sender: [u8; 32],
    payload: &AssignMultiply,
    now: u64,
) -> DispatchPath {
    if !cfg.enabled {
        return DispatchPath::Queue {
            cap: "auto-mode-disabled",
            reason: "auto-accept-orchestrator flag is off".into(),
        };
    }
    if !sender_matches_orchestrator(orchestrator, sender) {
        return DispatchPath::Queue {
            cap: "non-orchestrator-sender",
            reason: "sender does not match configured orchestrator".into(),
        };
    }
    match gate_assign_multiply(cfg, state, payload, now) {
        GateDecision::Accept {
            usd_lamports,
            label,
        } => DispatchPath::AutoExecute {
            usd_lamports,
            label,
        },
        GateDecision::FallThrough { cap, reason } => DispatchPath::Queue { cap, reason },
    }
}

/// Wall-clock helper.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Returns true iff `sender` is the configured orchestrator. Uses
/// constant-time-ish comparison only at the array level; the real
/// security model is "the orchestrator allowlist already filters
/// upstream of this gate" — sender_is_authorised in dispatch.rs runs
/// before auto-mode is consulted. This helper is the final defensive
/// check: even with the allowlist disabled (devnet) we refuse to
/// auto-accept from a wildcard sender.
pub fn sender_matches_orchestrator(orchestrator: Option<[u8; 32]>, sender: [u8; 32]) -> bool {
    match orchestrator {
        Some(o) => o == sender,
        // Auto-mode requires a configured orchestrator to be meaningful.
        // Devnet sandbox (orchestrator=None) deliberately can't auto-accept;
        // every envelope still goes through the normal queue path.
        None => false,
    }
}

/// Auto-mode gate for AssignMultiply. Decides whether to bypass the
/// approval queue or fall through. Pure function for testability — the
/// dispatcher uses [`now_unix_secs`] to feed `now`.
pub fn gate_assign_multiply(
    cfg: &AutoModeConfig,
    state: &AutoModeState,
    payload: &AssignMultiply,
    now: u64,
) -> GateDecision {
    if !cfg.enabled {
        return GateDecision::FallThrough {
            cap: "auto-mode-disabled",
            reason: "auto-accept-orchestrator flag is off".into(),
        };
    }

    // Multiply-specific gate 1: target_ltv ceiling. Refuse auto-accept
    // if the orchestrator is asking for too much leverage. Tightly
    // bounded since each leverage tick walks a Kamino loop that's
    // expensive to unwind.
    if payload.target_ltv_bps > cfg.max_target_ltv_bps {
        return GateDecision::FallThrough {
            cap: "target_ltv",
            reason: format!(
                "target_ltv_bps={} exceeds auto-mode cap {}",
                payload.target_ltv_bps, cfg.max_target_ltv_bps
            ),
        };
    }

    // Multiply-specific gate 2: full deleverage escape hatch. When the
    // orchestrator says "carry inverted, unwind", `target_ltv_bps = 0`
    // is the most common signal. Deleverage to safety should never be
    // cap-blocked — letting it through unconditionally is the right
    // safety default.
    let is_full_deleverage = payload.target_ltv_bps == 0;

    // Cooldown gate — applies to every auto-accept including deleverage,
    // because two deleverage envelopes within 60s would usually be a
    // duplicate/retry rather than a genuine new signal.
    let since_last = state.secs_since_last_accept_at(now);
    if since_last < cfg.cooldown_secs {
        return GateDecision::FallThrough {
            cap: "cooldown",
            reason: format!(
                "cooldown active: {}s since last auto-accept, need >= {}s",
                since_last, cfg.cooldown_secs
            ),
        };
    }

    // Multiply doesn't carry a USD field; size it conservatively by the
    // daemon's per-CLI max-position. The cumulative cap therefore caps
    // the *number* of cap-sized leverage rounds per day, not their net
    // USD effect. Pragmatic for the M11 ship — the operator can disable
    // the cumulative cap by raising it above `max_position * (24h /
    // cooldown)` if they want unrestricted auto-mode on multiply, which
    // is documented in the runbook.
    //
    // For the single-action cap check on multiply, we use a synthetic
    // unit of 0 — multiply's only single-action knob is `target_ltv_bps`,
    // already checked above. The cumulative cap is what bounds frequency.
    let synthetic_size_usd_lamports = cfg.max_single_action_usd_lamports;

    // Single-action cap is a no-op for multiply when synthetic equals
    // the cap (always passes by construction). Kept here so future
    // payload-evolution (target_ltv → usd) drops in cleanly.
    if synthetic_size_usd_lamports > cfg.max_single_action_usd_lamports {
        return GateDecision::FallThrough {
            cap: "single_action_usd",
            reason: format!(
                "single-action USD {} exceeds cap {}",
                synthetic_size_usd_lamports, cfg.max_single_action_usd_lamports
            ),
        };
    }

    // Cumulative cap. Full-deleverage bypasses (escape hatch) when
    // allow_deleverage_always is on.
    if !(is_full_deleverage && cfg.allow_deleverage_always) {
        let cumulative = state.cumulative_24h_at(now);
        if cumulative.saturating_add(synthetic_size_usd_lamports)
            > cfg.max_cumulative_24h_usd_lamports
        {
            return GateDecision::FallThrough {
                cap: "cumulative_24h",
                reason: format!(
                    "cumulative 24h USD {} + this {} would exceed cap {}",
                    cumulative,
                    synthetic_size_usd_lamports,
                    cfg.max_cumulative_24h_usd_lamports
                ),
            };
        }
    }

    GateDecision::Accept {
        usd_lamports: synthetic_size_usd_lamports,
        label: if is_full_deleverage {
            "AssignMultiply(full-deleverage)"
        } else {
            "AssignMultiply"
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign(target_ltv: u16, slippage: u16) -> AssignMultiply {
        AssignMultiply {
            vault: [0; 32],
            target_ltv_bps: target_ltv,
            max_slippage_bps: slippage,
            deadline_unix: 0,
        }
    }

    fn cfg_enabled() -> AutoModeConfig {
        let mut c = AutoModeConfig::default();
        c.enabled = true;
        c
    }

    #[test]
    fn disabled_always_falls_through() {
        let cfg = AutoModeConfig::default(); // enabled=false
        let st = AutoModeState::new();
        match gate_assign_multiply(&cfg, &st, &assign(6000, 50), 1_000) {
            GateDecision::FallThrough { cap, .. } => assert_eq!(cap, "auto-mode-disabled"),
            _ => panic!("expected FallThrough when disabled"),
        }
    }

    #[test]
    fn under_caps_accepts() {
        let cfg = cfg_enabled();
        let st = AutoModeState::new();
        match gate_assign_multiply(&cfg, &st, &assign(6000, 50), 1_000) {
            GateDecision::Accept { label, .. } => assert_eq!(label, "AssignMultiply"),
            _ => panic!("expected Accept"),
        }
    }

    #[test]
    fn target_ltv_above_cap_falls_through() {
        let cfg = cfg_enabled();
        let st = AutoModeState::new();
        // 6501 > 6500 default
        match gate_assign_multiply(&cfg, &st, &assign(6501, 50), 1_000) {
            GateDecision::FallThrough { cap, .. } => assert_eq!(cap, "target_ltv"),
            _ => panic!("expected target_ltv fall-through"),
        }
    }

    #[test]
    fn cooldown_blocks_within_window() {
        let cfg = cfg_enabled();
        let st = AutoModeState::new();
        // First accept at t=1000 — should pass
        let _ = gate_assign_multiply(&cfg, &st, &assign(6000, 50), 1_000);
        st.record_at(1_000, cfg.max_single_action_usd_lamports);
        // Second at t=1030 — within 60s cooldown
        match gate_assign_multiply(&cfg, &st, &assign(6000, 50), 1_030) {
            GateDecision::FallThrough { cap, .. } => assert_eq!(cap, "cooldown"),
            _ => panic!("expected cooldown fall-through"),
        }
        // Third at t=1100 — past cooldown
        match gate_assign_multiply(&cfg, &st, &assign(6000, 50), 1_100) {
            GateDecision::Accept { .. } => (),
            _ => panic!("expected Accept past cooldown"),
        }
    }

    #[test]
    fn cumulative_cap_blocks() {
        let mut cfg = cfg_enabled();
        // Set cumulative to single_action * 2 so two accepts fit, third doesn't.
        cfg.max_cumulative_24h_usd_lamports = cfg.max_single_action_usd_lamports * 2;
        cfg.cooldown_secs = 0; // disable cooldown for this test
        let st = AutoModeState::new();
        // Manually record two prior accepts
        st.record_at(1_000, cfg.max_single_action_usd_lamports);
        st.record_at(1_001, cfg.max_single_action_usd_lamports);
        // Third would push cumulative to 3x single_action > cap
        match gate_assign_multiply(&cfg, &st, &assign(6000, 50), 1_002) {
            GateDecision::FallThrough { cap, .. } => assert_eq!(cap, "cumulative_24h"),
            _ => panic!("expected cumulative_24h fall-through"),
        }
    }

    #[test]
    fn full_deleverage_bypasses_cumulative_cap() {
        let mut cfg = cfg_enabled();
        cfg.cooldown_secs = 0;
        let st = AutoModeState::new();
        // Saturate the cumulative cap completely.
        st.record_at(1_000, cfg.max_cumulative_24h_usd_lamports);
        // target_ltv_bps=0 is full deleverage — escape hatch.
        match gate_assign_multiply(&cfg, &st, &assign(0, 50), 1_001) {
            GateDecision::Accept { label, .. } => {
                assert_eq!(label, "AssignMultiply(full-deleverage)");
            }
            _ => panic!("expected Accept for full deleverage"),
        }
    }

    #[test]
    fn full_deleverage_does_not_bypass_cooldown() {
        let cfg = cfg_enabled();
        let st = AutoModeState::new();
        // Record a fresh prior accept that puts us in cooldown.
        st.record_at(1_000, 0);
        // Even a full deleverage should observe cooldown — two deleverages
        // within 60s is a duplicate/retry, not a fresh signal.
        match gate_assign_multiply(&cfg, &st, &assign(0, 50), 1_030) {
            GateDecision::FallThrough { cap, .. } => assert_eq!(cap, "cooldown"),
            _ => panic!("expected cooldown to block even deleverage"),
        }
    }

    #[test]
    fn full_deleverage_can_be_disallowed() {
        let mut cfg = cfg_enabled();
        cfg.cooldown_secs = 0;
        cfg.allow_deleverage_always = false;
        let st = AutoModeState::new();
        // Saturate the cumulative cap.
        st.record_at(1_000, cfg.max_cumulative_24h_usd_lamports);
        match gate_assign_multiply(&cfg, &st, &assign(0, 50), 1_001) {
            GateDecision::FallThrough { cap, .. } => assert_eq!(cap, "cumulative_24h"),
            _ => panic!("expected cumulative cap to apply when deleverage exception is off"),
        }
    }

    #[test]
    fn window_prunes_entries_older_than_24h() {
        let st = AutoModeState::new();
        st.record_at(0, 1_000);
        st.record_at(1, 2_000);
        // At now = WINDOW_SECS + 1, both entries are older than the cutoff
        // (cutoff = now - WINDOW_SECS = 1 → entries at t=0 evicted, t=1 kept).
        assert_eq!(st.cumulative_24h_at(WINDOW_SECS + 1), 2_000);
        // At now = WINDOW_SECS + 2, both pruned.
        assert_eq!(st.cumulative_24h_at(WINDOW_SECS + 2), 0);
    }

    #[test]
    fn sender_matches_orchestrator_logic() {
        let orch = [7u8; 32];
        let other = [9u8; 32];
        assert!(sender_matches_orchestrator(Some(orch), orch));
        assert!(!sender_matches_orchestrator(Some(orch), other));
        assert!(!sender_matches_orchestrator(None, orch));
    }

    #[test]
    fn secs_since_last_with_no_record_is_max() {
        let st = AutoModeState::new();
        assert_eq!(st.secs_since_last_accept_at(1_000), u64::MAX);
    }

    #[test]
    fn secs_since_last_after_record() {
        let st = AutoModeState::new();
        st.record_at(1_000, 100);
        assert_eq!(st.secs_since_last_accept_at(1_060), 60);
    }
}

#[cfg(test)]
mod dispatch_path_tests {
    //! End-to-end (within auto_mode) tests for `decide_assign_multiply`.
    //! These exercise the per-task spec scenarios:
    //!   * auto_accept_under_caps
    //!   * auto_accept_falls_through_above_single_cap (synthetic — not
    //!     reachable for AssignMultiply but pinned for future-proofing)
    //!   * auto_accept_falls_through_above_cumulative_cap
    //!   * auto_accept_cooldown_falls_through
    //!   * auto_accept_rejects_non_orchestrator_sender
    //!   * auto_accept_off_by_default
    //!   * auto_accept_always_allows_full_deleverage
    //!   * auto_accept_rejects_target_ltv_above_cap
    use super::*;

    const ORCH: [u8; 32] = [7u8; 32];
    const OTHER: [u8; 32] = [9u8; 32];

    fn assign(target_ltv: u16) -> AssignMultiply {
        AssignMultiply {
            vault: [0; 32],
            target_ltv_bps: target_ltv,
            max_slippage_bps: 50,
            deadline_unix: 0,
        }
    }

    fn cfg_on() -> AutoModeConfig {
        AutoModeConfig {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn auto_accept_under_caps() {
        let cfg = cfg_on();
        let st = AutoModeState::new();
        match decide_assign_multiply(&cfg, &st, Some(ORCH), ORCH, &assign(6000), 1_000) {
            DispatchPath::AutoExecute { label, .. } => assert_eq!(label, "AssignMultiply"),
            other => panic!("expected AutoExecute, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_falls_through_above_cumulative_cap() {
        let mut cfg = cfg_on();
        cfg.cooldown_secs = 0;
        cfg.max_cumulative_24h_usd_lamports = cfg.max_single_action_usd_lamports * 2;
        let st = AutoModeState::new();
        st.record_at(1_000, cfg.max_single_action_usd_lamports);
        st.record_at(1_001, cfg.max_single_action_usd_lamports);
        match decide_assign_multiply(&cfg, &st, Some(ORCH), ORCH, &assign(6000), 1_002) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "cumulative_24h"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_cooldown_falls_through() {
        let cfg = cfg_on();
        let st = AutoModeState::new();
        st.record_at(1_000, cfg.max_single_action_usd_lamports);
        match decide_assign_multiply(&cfg, &st, Some(ORCH), ORCH, &assign(6000), 1_030) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "cooldown"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_rejects_non_orchestrator_sender() {
        let cfg = cfg_on();
        let st = AutoModeState::new();
        // Even with all caps satisfied, a non-orchestrator sender must queue.
        match decide_assign_multiply(&cfg, &st, Some(ORCH), OTHER, &assign(6000), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "non-orchestrator-sender"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_off_by_default() {
        let cfg = AutoModeConfig::default(); // enabled=false
        let st = AutoModeState::new();
        match decide_assign_multiply(&cfg, &st, Some(ORCH), ORCH, &assign(6000), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "auto-mode-disabled"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_always_allows_full_deleverage() {
        let mut cfg = cfg_on();
        cfg.cooldown_secs = 0;
        let st = AutoModeState::new();
        // Saturate the cumulative cap so a normal Assign would fall through.
        st.record_at(1_000, cfg.max_cumulative_24h_usd_lamports);
        match decide_assign_multiply(&cfg, &st, Some(ORCH), ORCH, &assign(0), 1_001) {
            DispatchPath::AutoExecute { label, .. } => {
                assert_eq!(label, "AssignMultiply(full-deleverage)");
            }
            other => panic!("expected AutoExecute for deleverage, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_rejects_target_ltv_above_cap() {
        let cfg = cfg_on(); // max_target_ltv_bps default = 6500
        let st = AutoModeState::new();
        match decide_assign_multiply(&cfg, &st, Some(ORCH), ORCH, &assign(6501), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "target_ltv"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_falls_through_above_single_cap_is_structurally_unreachable() {
        // Multiply has no USD field on AssignMultiply — the single-action
        // USD cap is bypassed by construction (synthetic_size ==
        // max_single_action). This test pins that pinning: even at the
        // largest reasonable target_ltv the daemon will accept, the cap is
        // never the gating factor for multiply. (target_ltv_bps > cap is
        // a separate gate — see `auto_accept_rejects_target_ltv_above_cap`.)
        //
        // If a future protocol revision adds a USD field to AssignMultiply,
        // this test should fail and be replaced with a real "single-action
        // cap blown → queues" assertion.
        let mut cfg = cfg_on();
        cfg.cooldown_secs = 0;
        // Make the single-action cap absurdly tiny — for multiply this
        // still doesn't trigger because the synthetic size IS the cap.
        cfg.max_single_action_usd_lamports = 1;
        cfg.max_cumulative_24h_usd_lamports = u64::MAX;
        let st = AutoModeState::new();
        match decide_assign_multiply(&cfg, &st, Some(ORCH), ORCH, &assign(6000), 1_000) {
            DispatchPath::AutoExecute { .. } => (), // expected — no USD field to compare
            other => panic!("multiply has no USD field to gate on: {other:?}"),
        }
    }

    #[test]
    fn auto_accept_with_no_orchestrator_configured_falls_through() {
        // Defence-in-depth: if --orchestrator-agent-id is omitted (devnet
        // sandbox), auto-mode must NEVER fire — even with the flag on.
        let cfg = cfg_on();
        let st = AutoModeState::new();
        match decide_assign_multiply(&cfg, &st, None, ORCH, &assign(6000), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "non-orchestrator-sender"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }
}
