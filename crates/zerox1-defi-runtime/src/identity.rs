//! Long-lived agent identity, keyed by role rather than instance.
//!
//! A `RoleIdentity` survives any single daemon process. When a daemon
//! crashes and a replacement boots elsewhere, the replacement reloads
//! the same role key from a secrets backend and inherits the role's
//! place on the mesh.
//!
//! The role's private key is stored as raw 32-byte Ed25519 seed bytes
//! rather than a typed `SigningKey`. This keeps `zerox1-defi-runtime`
//! free of an ed25519-dalek dependency: solana-sdk transitively pulls
//! ed25519-dalek v1 with `zeroize <1.4`, while `zerox1-protocol`
//! (which performs envelope signing) uses ed25519-dalek v2 with
//! `zeroize ^1.5` — the two would collide if both were direct deps
//! here. Daemons reconstruct a typed `SigningKey` at the call site
//! when handing the seed into `zerox1-protocol`.

use serde::{Deserialize, Serialize};

/// The six roles in the 01fi fleet. Five executing desks plus the
/// orchestrator (mobile PM).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    Multiply,
    HedgedJlp,
    StableFloor,
    RiskWatcher,
    Researcher,
    Orchestrator,
}

impl Role {
    /// Stable lowercase identifier used in logs, file names, and config.
    /// Part of the wire / config contract — changes here are breaking.
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::Multiply     => "multiply",
            Role::HedgedJlp    => "hedgedjlp",
            Role::StableFloor  => "stablefloor",
            Role::RiskWatcher  => "riskwatcher",
            Role::Researcher   => "researcher",
            Role::Orchestrator => "orchestrator",
        }
    }

    /// Parse a stable identifier back into a Role. Case-insensitive.
    pub fn from_str_lowercase(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "multiply"     => Some(Role::Multiply),
            "hedgedjlp"    => Some(Role::HedgedJlp),
            "stablefloor"  => Some(Role::StableFloor),
            "riskwatcher"  => Some(Role::RiskWatcher),
            "researcher"   => Some(Role::Researcher),
            "orchestrator" => Some(Role::Orchestrator),
            _ => None,
        }
    }
}

/// A role's long-lived signing identity.
///
/// The signing key is stored as raw 32-byte Ed25519 seed bytes. Callers
/// that need a typed `SigningKey` (e.g., to sign an `Envelope` via
/// `zerox1-protocol`) reconstruct it from `signing_key_bytes()`.
#[derive(Debug, Clone)]
pub struct RoleIdentity {
    role: Role,
    signing_key_seed: [u8; 32],
}

impl RoleIdentity {
    pub fn new(role: Role, signing_key_seed: [u8; 32]) -> Self {
        Self { role, signing_key_seed }
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// Raw 32-byte Ed25519 seed (private). Hand to crypto-aware crates
    /// (e.g., `zerox1-protocol`) that own ed25519-dalek directly.
    pub fn signing_key_bytes(&self) -> &[u8; 32] {
        &self.signing_key_seed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_str_stable() {
        assert_eq!(Role::Multiply.as_str(), "multiply");
        assert_eq!(Role::HedgedJlp.as_str(), "hedgedjlp");
        assert_eq!(Role::StableFloor.as_str(), "stablefloor");
        assert_eq!(Role::RiskWatcher.as_str(), "riskwatcher");
        assert_eq!(Role::Researcher.as_str(), "researcher");
        assert_eq!(Role::Orchestrator.as_str(), "orchestrator");
    }

    #[test]
    fn from_str_round_trips() {
        for r in [
            Role::Multiply, Role::HedgedJlp, Role::StableFloor,
            Role::RiskWatcher, Role::Researcher, Role::Orchestrator,
        ] {
            assert_eq!(Role::from_str_lowercase(r.as_str()), Some(r));
        }
        assert_eq!(Role::from_str_lowercase("MULTIPLY"), Some(Role::Multiply));
        assert_eq!(Role::from_str_lowercase("nonsense"), None);
    }

    #[test]
    fn identity_holds_seed_unmodified() {
        let seed = [42u8; 32];
        let id = RoleIdentity::new(Role::Multiply, seed);
        assert_eq!(id.role(), Role::Multiply);
        assert_eq!(id.signing_key_bytes(), &seed);
    }
}
