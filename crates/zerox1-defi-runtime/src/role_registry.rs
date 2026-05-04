//! Mesh-level role-to-instance resolution.
//!
//! Each daemon broadcasts a `Beacon` envelope every N seconds containing
//! its role and instance pubkey (the libp2p identity, separate from the
//! daemon's long-lived role key — see `runtime::identity`). The registry
//! maintains a `Role -> (instance_pubkey, last_seen)` map. Outbound
//! mesh sends look up the current holder of a role via `resolve()`.
//!
//! Conflict resolution is deterministic: when two BEACONs claim the
//! same role with different instance pubkeys within the staleness
//! window, the lexicographically lower pubkey wins. The losing
//! daemon detects the conflict on its own via its own registry's
//! `observe()` return value and is expected to shut down (or yield
//! and retry once the winner stops broadcasting).
//!
//! After the staleness window expires (no fresh BEACONs from the
//! current holder), a new claim from any pubkey is accepted.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::identity::Role;

/// Raw Ed25519 public-key bytes. The runtime crate stores pubkeys as
/// raw bytes to avoid pulling ed25519-dalek as a direct dep — see
/// `runtime::identity` for the reasoning.
pub type InstanceKey = [u8; 32];

#[derive(Debug, Clone, Copy)]
pub struct RoleAssignment {
    pub instance_pubkey: InstanceKey,
    pub last_seen: Instant,
}

pub struct RoleRegistry {
    map: RwLock<HashMap<Role, RoleAssignment>>,
    stale_after: Duration,
}

impl RoleRegistry {
    /// Create a new registry. `stale_after` is the BEACON staleness
    /// window — typically 3× the daemon's BEACON interval, so two
    /// missed BEACONs aren't enough to evict a holder.
    pub fn new(stale_after: Duration) -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
            stale_after,
        }
    }

    /// Update the registry from an observed BEACON. Returns
    /// `Some(loser_pubkey)` if a live conflict was detected — the
    /// caller can use this signal (e.g., compare it to its own
    /// instance pubkey) to decide whether to shut down.
    ///
    /// Decision tree:
    /// - No prior holder → accept, return None
    /// - Same instance as before → refresh last_seen, return None
    /// - Prior holder went stale → accept, return None
    /// - Live conflict → lower pubkey wins; loser is returned
    pub fn observe(&self, role: Role, instance_pubkey: InstanceKey) -> Option<InstanceKey> {
        let mut map = self.map.write().unwrap();
        let now = Instant::now();
        match map.get(&role) {
            None => {
                map.insert(role, RoleAssignment { instance_pubkey, last_seen: now });
                None
            }
            Some(existing) if existing.instance_pubkey == instance_pubkey => {
                map.insert(role, RoleAssignment { instance_pubkey, last_seen: now });
                None
            }
            Some(existing) if existing.last_seen.elapsed() > self.stale_after => {
                map.insert(role, RoleAssignment { instance_pubkey, last_seen: now });
                None
            }
            Some(existing) => {
                // Live conflict. Lower pubkey wins.
                if instance_pubkey < existing.instance_pubkey {
                    let loser = existing.instance_pubkey;
                    map.insert(role, RoleAssignment { instance_pubkey, last_seen: now });
                    Some(loser)
                } else {
                    Some(instance_pubkey)
                }
            }
        }
    }

    /// Look up the current holder of a role. Returns None if no claim
    /// has been observed or the most recent claim has gone stale.
    pub fn resolve(&self, role: Role) -> Option<InstanceKey> {
        let map = self.map.read().unwrap();
        map.get(&role)
            .filter(|a| a.last_seen.elapsed() <= self.stale_after)
            .map(|a| a.instance_pubkey)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> InstanceKey {
        [byte; 32]
    }

    #[test]
    fn first_claim_accepts() {
        let reg = RoleRegistry::new(Duration::from_secs(60));
        assert!(reg.observe(Role::Multiply, key(0xAA)).is_none());
        assert_eq!(reg.resolve(Role::Multiply), Some(key(0xAA)));
    }

    #[test]
    fn refresh_same_instance_no_conflict() {
        let reg = RoleRegistry::new(Duration::from_secs(60));
        assert!(reg.observe(Role::Multiply, key(0xAA)).is_none());
        assert!(reg.observe(Role::Multiply, key(0xAA)).is_none());
    }

    #[test]
    fn live_conflict_lower_pubkey_wins() {
        let reg = RoleRegistry::new(Duration::from_secs(60));
        let lower = key(0x10);
        let higher = key(0xF0);
        assert!(reg.observe(Role::Multiply, higher).is_none());
        let loser = reg.observe(Role::Multiply, lower);
        assert_eq!(loser, Some(higher));
        assert_eq!(reg.resolve(Role::Multiply), Some(lower));
    }

    #[test]
    fn live_conflict_higher_pubkey_loses() {
        let reg = RoleRegistry::new(Duration::from_secs(60));
        let lower = key(0x10);
        let higher = key(0xF0);
        assert!(reg.observe(Role::Multiply, lower).is_none());
        let loser = reg.observe(Role::Multiply, higher);
        // The higher pubkey is its own loser — it should yield to lower.
        assert_eq!(loser, Some(higher));
        // The registry still resolves to the original (lower) holder.
        assert_eq!(reg.resolve(Role::Multiply), Some(lower));
    }

    #[test]
    fn stale_holder_evicted_by_new_claim() {
        let reg = RoleRegistry::new(Duration::from_millis(10));
        assert!(reg.observe(Role::Multiply, key(0xAA)).is_none());
        std::thread::sleep(Duration::from_millis(20));
        // Stale: even a higher pubkey wins on a fresh claim.
        assert!(reg.observe(Role::Multiply, key(0xFF)).is_none());
        assert_eq!(reg.resolve(Role::Multiply), Some(key(0xFF)));
    }

    #[test]
    fn resolve_returns_none_when_stale() {
        let reg = RoleRegistry::new(Duration::from_millis(10));
        assert!(reg.observe(Role::Multiply, key(0xAA)).is_none());
        std::thread::sleep(Duration::from_millis(20));
        assert!(reg.resolve(Role::Multiply).is_none());
    }

    #[test]
    fn different_roles_dont_interfere() {
        let reg = RoleRegistry::new(Duration::from_secs(60));
        assert!(reg.observe(Role::Multiply, key(0x11)).is_none());
        assert!(reg.observe(Role::HedgedJlp, key(0x22)).is_none());
        assert_eq!(reg.resolve(Role::Multiply), Some(key(0x11)));
        assert_eq!(reg.resolve(Role::HedgedJlp), Some(key(0x22)));
    }
}
