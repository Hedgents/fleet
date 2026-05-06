//! Per-(kind, asset) emission throttle. Same threshold breach within the
//! cooldown window is suppressed unless severity escalates.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zerox1_protocol::fleet::researcher::{AssetId, SignalKind, SignalSeverity};

/// Default cooldown — same (kind, asset, severity) won't re-emit for 60s.
/// Severity escalation overrides cooldown (Notice -> Important still emits
/// even within window).
pub const DEFAULT_COOLDOWN_SECS: u64 = 60;

#[derive(Clone, Copy)]
struct Entry {
    last_emit: Instant,
    last_severity: SignalSeverity,
}

/// Internal HashMap key. SignalKind/AssetId in the protocol crate are not
/// derived Eq+Hash (they're #[repr(u16)] enums); we key by their u16
/// discriminants so this module doesn't have to widen the protocol's
/// derive footprint. The public API is still typed in terms of the
/// protocol enums.
type Key = (u16, u16);

fn key(kind: SignalKind, asset: AssetId) -> Key {
    (kind as u16, asset as u16)
}

pub struct EmissionTracker {
    inner: Mutex<HashMap<Key, Entry>>,
    cooldown: Duration,
}

impl EmissionTracker {
    pub fn new(cooldown: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            cooldown,
        }
    }

    pub fn default() -> Self {
        Self::new(Duration::from_secs(DEFAULT_COOLDOWN_SECS))
    }

    /// Returns `true` if the emission should proceed (not throttled),
    /// `false` if it should be suppressed. ALWAYS records the attempt.
    pub fn should_emit(
        &self,
        kind: SignalKind,
        asset: AssetId,
        severity: SignalSeverity,
    ) -> bool {
        let mut g = self.inner.lock().unwrap();
        let k = key(kind, asset);
        match g.get(&k) {
            Some(prev) => {
                // Severity escalation always emits.
                if (severity as u8) > (prev.last_severity as u8) {
                    g.insert(k, Entry { last_emit: Instant::now(), last_severity: severity });
                    true
                } else if prev.last_emit.elapsed() >= self.cooldown {
                    g.insert(k, Entry { last_emit: Instant::now(), last_severity: severity });
                    true
                } else {
                    false
                }
            }
            None => {
                g.insert(k, Entry { last_emit: Instant::now(), last_severity: severity });
                true
            }
        }
    }

    /// Test helper.
    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.inner.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_emission_passes() {
        let t = EmissionTracker::new(Duration::from_secs(60));
        assert!(t.should_emit(SignalKind::PerpFundingAbove, AssetId::SOL, SignalSeverity::Notice));
    }

    #[test]
    fn same_kind_same_asset_same_severity_throttled() {
        let t = EmissionTracker::new(Duration::from_secs(60));
        assert!(t.should_emit(SignalKind::PerpFundingAbove, AssetId::SOL, SignalSeverity::Notice));
        assert!(!t.should_emit(SignalKind::PerpFundingAbove, AssetId::SOL, SignalSeverity::Notice));
    }

    #[test]
    fn severity_escalation_overrides_throttle() {
        let t = EmissionTracker::new(Duration::from_secs(60));
        assert!(t.should_emit(SignalKind::PerpFundingAbove, AssetId::SOL, SignalSeverity::Notice));
        assert!(t.should_emit(SignalKind::PerpFundingAbove, AssetId::SOL, SignalSeverity::Important));
        // Going back DOWN to Notice within cooldown should still throttle.
        assert!(!t.should_emit(SignalKind::PerpFundingAbove, AssetId::SOL, SignalSeverity::Notice));
    }

    #[test]
    fn different_assets_isolated() {
        let t = EmissionTracker::new(Duration::from_secs(60));
        assert!(t.should_emit(SignalKind::PerpFundingAbove, AssetId::SOL, SignalSeverity::Notice));
        assert!(t.should_emit(SignalKind::PerpFundingAbove, AssetId::ETH, SignalSeverity::Notice));
        assert!(t.should_emit(SignalKind::PerpFundingAbove, AssetId::BTC, SignalSeverity::Notice));
    }

    #[test]
    fn cooldown_expiry_re_emits() {
        let t = EmissionTracker::new(Duration::from_millis(50));
        assert!(t.should_emit(SignalKind::PerpFundingAbove, AssetId::SOL, SignalSeverity::Notice));
        std::thread::sleep(Duration::from_millis(60));
        assert!(t.should_emit(SignalKind::PerpFundingAbove, AssetId::SOL, SignalSeverity::Notice));
    }
}
