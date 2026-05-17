//! Per-strategy cooldown tracking.
//!
//! Prevents the orchestrator from hot-looping the same strategy. After
//! an envelope is dispatched against a strategy, that strategy is in
//! cooldown for `cooldown_secs` and the tick loop suppresses any new
//! envelope to it. NoAction-style audit lines are still written so the
//! suppressed decision is recorded.
//!
//! Single-task ownership: `TickCtx` holds a `Mutex<CooldownTracker>`
//! and the tick task is the only consumer. The mutex is there so the
//! cooldown can be queried + updated atomically; there is no actual
//! contention.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Tracks the last-dispatch unix-second per strategy id.
#[derive(Debug, Default)]
pub struct CooldownTracker {
    last_action_unix: HashMap<String, u64>,
}

impl CooldownTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` if the strategy is still in cooldown at `now`,
    /// meaning the orchestrator should NOT emit a new envelope to it.
    /// A strategy that has never been actioned is never in cooldown.
    pub fn is_cooled_down(
        &self,
        strategy: &str,
        now: SystemTime,
        cooldown: Duration,
    ) -> bool {
        let Some(&last) = self.last_action_unix.get(strategy) else {
            return false;
        };
        let now_secs = unix_seconds(now);
        let elapsed = now_secs.saturating_sub(last);
        elapsed < cooldown.as_secs()
    }

    /// Record that an envelope was dispatched against `strategy` at `now`.
    pub fn record(&mut self, strategy: &str, now: SystemTime) {
        self.last_action_unix
            .insert(strategy.to_string(), unix_seconds(now));
    }

    /// Seconds since the last dispatch, or `None` if never dispatched.
    /// Used for audit log + diagnostic output.
    pub fn seconds_since(&self, strategy: &str, now: SystemTime) -> Option<u64> {
        let last = *self.last_action_unix.get(strategy)?;
        Some(unix_seconds(now).saturating_sub(last))
    }
}

fn unix_seconds(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn never_actioned_is_not_cooled_down() {
        let c = CooldownTracker::new();
        assert!(!c.is_cooled_down("stable_yield", t(1000), Duration::from_secs(300)));
    }

    #[test]
    fn just_actioned_is_cooled_down() {
        let mut c = CooldownTracker::new();
        c.record("stable_yield", t(1000));
        assert!(c.is_cooled_down("stable_yield", t(1100), Duration::from_secs(300)));
    }

    #[test]
    fn cooldown_expires() {
        let mut c = CooldownTracker::new();
        c.record("multiply", t(1000));
        // 300 seconds later, still cooled (boundary inclusive: elapsed = 300 < 300 is false)
        assert!(!c.is_cooled_down("multiply", t(1300), Duration::from_secs(300)));
        // 299 seconds later, still cooled
        assert!(c.is_cooled_down("multiply", t(1299), Duration::from_secs(300)));
    }

    #[test]
    fn cooldown_is_per_strategy() {
        let mut c = CooldownTracker::new();
        c.record("multiply", t(1000));
        assert!(c.is_cooled_down("multiply", t(1100), Duration::from_secs(300)));
        // Other strategies are unaffected.
        assert!(!c.is_cooled_down("stable_yield", t(1100), Duration::from_secs(300)));
    }

    #[test]
    fn seconds_since_returns_elapsed() {
        let mut c = CooldownTracker::new();
        c.record("hedgedjlp", t(1000));
        assert_eq!(c.seconds_since("hedgedjlp", t(1042)), Some(42));
        assert_eq!(c.seconds_since("multiply", t(1042)), None);
    }
}
