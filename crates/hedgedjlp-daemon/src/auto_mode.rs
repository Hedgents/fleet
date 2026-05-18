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
//! HedgedJLP owns two payload shapes:
//!   * AssignHedgedJlp — `usdc_lamports` is the size metric (6 decimals).
//!   * WithdrawHedgedJlp — `jlp_lamports`. JLP is not USDC-denominated;
//!     to keep the gate self-contained and avoid an extra chain read at
//!     gate time, hedged-JLP withdraws ALWAYS fall through to manual
//!     approval. The strategic intent is right too — unwinding a
//!     basis trade is high-blast-radius and should stay operator-gated
//!     until per-position USD reads are wired into the gate.
//!
//! 24h tracking is in-memory only (matches the existing rebalancer-state
//! pattern). On daemon restart the window resets; per-action caps still
//! bound individual envelopes per-tick.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use zerox1_protocol::fleet::hedgedjlp::{AssignHedgedJlp, WithdrawHedgedJlp};

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
}

impl Default for AutoModeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_single_action_usd_lamports: 50_000_000,        // $50
            max_cumulative_24h_usd_lamports: 200_000_000,      // $200
            cooldown_secs: 60,
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

/// Wall-clock helper.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Returns true iff `sender` is the configured orchestrator.
pub fn sender_matches_orchestrator(orchestrator: Option<[u8; 32]>, sender: [u8; 32]) -> bool {
    match orchestrator {
        Some(o) => o == sender,
        None => false,
    }
}

/// Outer decision — combines the orchestrator sender-match check with
/// the cap gate. Pure function for testability.
#[derive(Debug, PartialEq, Eq)]
pub enum DispatchPath {
    AutoExecute {
        usd_lamports: u64,
        label: &'static str,
    },
    Queue {
        cap: &'static str,
        reason: String,
    },
}

fn gate_common(
    cfg: &AutoModeConfig,
    state: &AutoModeState,
    usd_lamports: u64,
    label: &'static str,
    now: u64,
) -> DispatchPath {
    if usd_lamports > cfg.max_single_action_usd_lamports {
        return DispatchPath::Queue {
            cap: "single_action_usd",
            reason: format!(
                "{} size {} exceeds single-action cap {}",
                label, usd_lamports, cfg.max_single_action_usd_lamports
            ),
        };
    }

    let since_last = state.secs_since_last_accept_at(now);
    if since_last < cfg.cooldown_secs {
        return DispatchPath::Queue {
            cap: "cooldown",
            reason: format!(
                "cooldown active: {}s since last auto-accept, need >= {}s",
                since_last, cfg.cooldown_secs
            ),
        };
    }

    let cumulative = state.cumulative_24h_at(now);
    if cumulative.saturating_add(usd_lamports) > cfg.max_cumulative_24h_usd_lamports {
        return DispatchPath::Queue {
            cap: "cumulative_24h",
            reason: format!(
                "cumulative 24h USD {} + this {} would exceed cap {}",
                cumulative, usd_lamports, cfg.max_cumulative_24h_usd_lamports
            ),
        };
    }

    DispatchPath::AutoExecute {
        usd_lamports,
        label,
    }
}

/// Decide the dispatch path for an AssignHedgedJlp envelope.
pub fn decide_assign_hedgedjlp(
    cfg: &AutoModeConfig,
    state: &AutoModeState,
    orchestrator: Option<[u8; 32]>,
    sender: [u8; 32],
    payload: &AssignHedgedJlp,
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
    gate_common(cfg, state, payload.usdc_lamports, "AssignHedgedJlp", now)
}

/// Decide the dispatch path for a WithdrawHedgedJlp envelope.
///
/// HedgedJLP withdraws use `jlp_lamports` — not USDC — and unwinding a
/// basis trade is high-blast-radius (close shorts, sell JLP, settle).
/// To keep the gate self-contained and free of additional chain reads,
/// hedged-JLP withdraws ALWAYS fall through to manual approval. Future
/// work could read the obligation USD value and gate on that; for now,
/// the strategically right answer is operator-gated.
#[allow(unused_variables)]
pub fn decide_withdraw_hedgedjlp(
    cfg: &AutoModeConfig,
    state: &AutoModeState,
    orchestrator: Option<[u8; 32]>,
    sender: [u8; 32],
    payload: &WithdrawHedgedJlp,
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
    // Deliberate: hedged-JLP withdraws always fall through to manual.
    DispatchPath::Queue {
        cap: "withdraw-manual-only",
        reason: "WithdrawHedgedJlp uses jlp_lamports (not USD) — always manual approval".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORCH: [u8; 32] = [7u8; 32];
    const OTHER: [u8; 32] = [9u8; 32];

    fn assign(usdc: u64) -> AssignHedgedJlp {
        AssignHedgedJlp {
            usdc_lamports: usdc,
            target_delta_bps: 0,
            max_borrow_rate_bps: 3000,
            deadline_unix: 0,
        }
    }

    fn withdraw(jlp: u64) -> WithdrawHedgedJlp {
        WithdrawHedgedJlp {
            jlp_lamports: jlp,
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
        match decide_assign_hedgedjlp(&cfg, &st, Some(ORCH), ORCH, &assign(40_000_000), 1_000) {
            DispatchPath::AutoExecute { label, .. } => assert_eq!(label, "AssignHedgedJlp"),
            other => panic!("expected AutoExecute, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_falls_through_above_single_cap() {
        let cfg = cfg_on();
        let st = AutoModeState::new();
        match decide_assign_hedgedjlp(&cfg, &st, Some(ORCH), ORCH, &assign(60_000_000), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "single_action_usd"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_falls_through_above_cumulative_cap() {
        let mut cfg = cfg_on();
        cfg.cooldown_secs = 0;
        cfg.max_cumulative_24h_usd_lamports = 100_000_000;
        let st = AutoModeState::new();
        st.record_at(1_000, 50_000_000);
        st.record_at(1_001, 50_000_000);
        match decide_assign_hedgedjlp(&cfg, &st, Some(ORCH), ORCH, &assign(10_000_000), 1_002) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "cumulative_24h"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_cooldown_falls_through() {
        let cfg = cfg_on();
        let st = AutoModeState::new();
        st.record_at(1_000, 10_000_000);
        match decide_assign_hedgedjlp(&cfg, &st, Some(ORCH), ORCH, &assign(10_000_000), 1_030) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "cooldown"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_rejects_non_orchestrator_sender() {
        let cfg = cfg_on();
        let st = AutoModeState::new();
        match decide_assign_hedgedjlp(&cfg, &st, Some(ORCH), OTHER, &assign(10_000_000), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "non-orchestrator-sender"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_off_by_default() {
        let cfg = AutoModeConfig::default();
        let st = AutoModeState::new();
        match decide_assign_hedgedjlp(&cfg, &st, Some(ORCH), ORCH, &assign(10_000_000), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "auto-mode-disabled"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn auto_accept_with_no_orchestrator_configured_falls_through() {
        let cfg = cfg_on();
        let st = AutoModeState::new();
        match decide_assign_hedgedjlp(&cfg, &st, None, ORCH, &assign(10_000_000), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "non-orchestrator-sender"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn withdraw_always_falls_through() {
        // HedgedJLP withdraws unconditionally queue — JLP isn't
        // USD-denominated and the unwind is high-blast-radius.
        let cfg = cfg_on();
        let st = AutoModeState::new();
        match decide_withdraw_hedgedjlp(&cfg, &st, Some(ORCH), ORCH, &withdraw(10_000_000), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "withdraw-manual-only"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn withdraw_full_sentinel_also_falls_through() {
        let cfg = cfg_on();
        let st = AutoModeState::new();
        match decide_withdraw_hedgedjlp(&cfg, &st, Some(ORCH), ORCH, &withdraw(u64::MAX), 1_000) {
            DispatchPath::Queue { cap, .. } => assert_eq!(cap, "withdraw-manual-only"),
            other => panic!("expected Queue, got {other:?}"),
        }
    }

    #[test]
    fn window_prunes_entries_older_than_24h() {
        let st = AutoModeState::new();
        st.record_at(0, 1_000);
        st.record_at(1, 2_000);
        assert_eq!(st.cumulative_24h_at(WINDOW_SECS + 1), 2_000);
        assert_eq!(st.cumulative_24h_at(WINDOW_SECS + 2), 0);
    }
}
