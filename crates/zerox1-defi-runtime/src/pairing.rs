//! Fleet pairing: HMAC-signed join-request / accept-join messages.
//!
//! See `01fi/FLEET_PAIRING.md` for the full protocol spec. This module
//! implements the *transport-agnostic* core:
//!
//! - Stable wire format (Borsh-friendly canonical JSON ordering)
//! - HMAC-SHA256 over the canonical body bytes
//! - State machine: `Unpaired → Pairing → Paired`
//! - Pure functions: build, sign, verify
//! - In-memory state guarded by a mutex; persisted via `persistence` module
//!
//! Transport (libp2p mesh, HTTP bridge, raw stdin) is not this module's
//! concern. Endpoints in `handlers::fleet` produce the bytes and accept
//! external bytes; the publisher chooses how to move them.

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;

type HmacSha256 = Hmac<Sha256>;

// ── Roles ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Orchestrator,
    Multiply,
    HedgedJlp,
    StableFloor,
    RiskWatcher,
    Researcher,
    Speculator,
}

impl Role {
    pub fn from_str(s: &str) -> Result<Self, PairingError> {
        match s.to_ascii_lowercase().as_str() {
            "orchestrator" => Ok(Role::Orchestrator),
            "multiply"     => Ok(Role::Multiply),
            "hedgedjlp" | "hedged_jlp" | "hedged-jlp" => Ok(Role::HedgedJlp),
            "stablefloor" | "stable_floor" | "stable-floor" => Ok(Role::StableFloor),
            "riskwatcher" | "risk_watcher" | "risk-watcher" => Ok(Role::RiskWatcher),
            "researcher"   => Ok(Role::Researcher),
            "speculator"   => Ok(Role::Speculator),
            other => Err(PairingError::UnknownRole(other.to_string())),
        }
    }
}

// ── Identity ────────────────────────────────────────────────────────────────

/// Long-lived fleet credentials this daemon was launched with.
/// Token must never be logged or returned over HTTP — only HMACs of bodies.
#[derive(Clone)]
pub struct FleetIdentity {
    pub fleet_id: [u8; 8],
    pub fleet_token: [u8; 32],
    pub role: Role,
    pub agent_id: String, // base58 Solana pubkey
}

impl FleetIdentity {
    /// Discovery topic slug = first 16 hex chars of sha256(fleet_token).
    pub fn discovery_topic(&self) -> String {
        use sha2::Digest;
        let h = Sha256::digest(self.fleet_token);
        format!("fleet/{}", hex::encode(&h[..8]))
    }
}

// ── Messages ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum FleetMessage {
    JoinRequest(JoinRequestBody),
    AcceptJoin(AcceptJoinBody),
    Revoke(RevokeBody),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JoinRequestBody {
    pub fleet_id: String,
    pub agent_id: String,
    pub role: Role,
    pub capabilities: Vec<String>,
    pub version: String,
    pub host_hint: String,
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AcceptJoinBody {
    pub fleet_id: String,
    pub orchestrator_agent_id: String,
    pub worker_agent_id: String,
    pub ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RevokeBody {
    pub fleet_id: String,
    pub revoked_agent_id: String,
    pub ts: u64,
}

/// Wire-format envelope: canonical JSON body + HMAC-hex.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedEnvelope {
    pub body: serde_json::Value,
    pub hmac: String,
}

// ── Sign / verify ───────────────────────────────────────────────────────────

/// Canonicalize a JSON value (sort keys recursively) so the bytes used for
/// HMAC are deterministic across publishers.
fn canonical_json(value: &serde_json::Value) -> Vec<u8> {
    fn walk(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), walk(&m[k]));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(walk).collect())
            }
            other => other.clone(),
        }
    }
    serde_json::to_vec(&walk(value)).expect("canonical_json serialize")
}

pub fn sign_body(token: &[u8; 32], body: &serde_json::Value) -> String {
    let bytes = canonical_json(body);
    let mut mac = HmacSha256::new_from_slice(token).expect("hmac key");
    mac.update(&bytes);
    hex::encode(mac.finalize().into_bytes())
}

pub fn verify_body(token: &[u8; 32], body: &serde_json::Value, hmac_hex: &str) -> bool {
    let bytes = canonical_json(body);
    let provided = match hex::decode(hmac_hex) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let mut mac = HmacSha256::new_from_slice(token).expect("hmac key");
    mac.update(&bytes);
    mac.verify_slice(&provided).is_ok()
}

// ── Builders ────────────────────────────────────────────────────────────────

pub fn build_join_request(
    identity: &FleetIdentity,
    capabilities: Vec<String>,
    host_hint: String,
    version: String,
    ts: u64,
) -> SignedEnvelope {
    let body = FleetMessage::JoinRequest(JoinRequestBody {
        fleet_id: hex::encode(identity.fleet_id),
        agent_id: identity.agent_id.clone(),
        role: identity.role,
        capabilities,
        version,
        host_hint,
        ts,
    });
    let body_value = serde_json::to_value(&body).expect("serialize join_request");
    let hmac = sign_body(&identity.fleet_token, &body_value);
    SignedEnvelope { body: body_value, hmac }
}

#[allow(dead_code)] // used by tests; called by orchestrator-side wiring (future)
pub fn build_accept_join(
    identity: &FleetIdentity,
    orchestrator_agent_id: String,
    worker_agent_id: String,
    ts: u64,
) -> SignedEnvelope {
    let body = FleetMessage::AcceptJoin(AcceptJoinBody {
        fleet_id: hex::encode(identity.fleet_id),
        orchestrator_agent_id,
        worker_agent_id,
        ts,
    });
    let body_value = serde_json::to_value(&body).expect("serialize accept_join");
    let hmac = sign_body(&identity.fleet_token, &body_value);
    SignedEnvelope { body: body_value, hmac }
}

// ── State machine ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PairingState {
    Unpaired,
    Pairing { sent_join_request_at: u64 },
    Paired { orchestrator_agent_id: String, paired_at: u64 },
    Revoked { revoked_at: u64 },
}

impl PairingState {
    #[allow(dead_code)] // called by upcoming PROPOSE filter middleware
    pub fn is_paired_with(&self, agent_id: &str) -> bool {
        matches!(self, PairingState::Paired { orchestrator_agent_id, .. } if orchestrator_agent_id == agent_id)
    }

    /// Accessor used by `mesh::sign_envelope` / `verify_envelope`.
    ///
    /// `PairingState` itself does not carry the fleet token (it lives on
    /// `FleetIdentity`); callers that hold both should pair them up before
    /// invoking the mesh helpers. This accessor exists so the mesh module
    /// compiles against a state-only API; it returns `None` by default.
    pub fn fleet_token(&self) -> Option<&str> {
        None
    }
}

/// Apply an inbound `accept_join` envelope to current state. Returns the new
/// state on success. Verifies HMAC, fleet_id match, target-worker match, and
/// monotonicity (no downgrade from Paired with a different orchestrator).
pub fn apply_accept_join(
    identity: &FleetIdentity,
    envelope: &SignedEnvelope,
    current: &PairingState,
) -> Result<PairingState, PairingError> {
    if !verify_body(&identity.fleet_token, &envelope.body, &envelope.hmac) {
        return Err(PairingError::BadHmac);
    }
    let msg: FleetMessage = serde_json::from_value(envelope.body.clone())
        .map_err(|e| PairingError::Malformed(e.to_string()))?;
    let body = match msg {
        FleetMessage::AcceptJoin(b) => b,
        _ => return Err(PairingError::WrongMessageKind),
    };
    if body.fleet_id != hex::encode(identity.fleet_id) {
        return Err(PairingError::WrongFleet);
    }
    if body.worker_agent_id != identity.agent_id {
        return Err(PairingError::WrongWorker);
    }
    if let PairingState::Paired { orchestrator_agent_id, .. } = current {
        if orchestrator_agent_id != &body.orchestrator_agent_id {
            return Err(PairingError::AlreadyPairedDifferent);
        }
    }
    Ok(PairingState::Paired {
        orchestrator_agent_id: body.orchestrator_agent_id,
        paired_at: body.ts,
    })
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PairingError {
    #[error("unknown role: {0}")]
    UnknownRole(String),
    #[error("HMAC verification failed")]
    BadHmac,
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("wrong message kind for this endpoint")]
    WrongMessageKind,
    #[error("envelope is for a different fleet_id")]
    WrongFleet,
    #[error("envelope targets a different worker agent_id")]
    WrongWorker,
    #[error("already paired with a different orchestrator; revoke first")]
    AlreadyPairedDifferent,
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(role: Role) -> FleetIdentity {
        FleetIdentity {
            fleet_id: [1, 2, 3, 4, 5, 6, 7, 8],
            fleet_token: [9; 32],
            role,
            agent_id: "WorkerAgentIdBase58".to_string(),
        }
    }

    #[test]
    fn discovery_topic_is_deterministic_and_hides_token() {
        let id = ident(Role::Multiply);
        let t1 = id.discovery_topic();
        let t2 = id.discovery_topic();
        assert_eq!(t1, t2);
        assert!(t1.starts_with("fleet/"));
        // Topic must not contain the raw token (anywhere)
        assert!(!t1.contains(&hex::encode(id.fleet_token)));
    }

    #[test]
    fn discovery_topic_changes_with_token() {
        let mut a = ident(Role::Multiply);
        let mut b = ident(Role::Multiply);
        a.fleet_token = [1; 32];
        b.fleet_token = [2; 32];
        assert_ne!(a.discovery_topic(), b.discovery_topic());
    }

    #[test]
    fn sign_then_verify_roundtrips() {
        let id = ident(Role::Multiply);
        let env = build_join_request(
            &id, vec!["kamino_supply".into()],
            "test-host".into(), "0.1.0".into(), 1234567,
        );
        assert!(verify_body(&id.fleet_token, &env.body, &env.hmac));
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let id = ident(Role::Multiply);
        let env = build_join_request(
            &id, vec![], "test".into(), "0.1.0".into(), 100,
        );
        let mut tampered = env.body.clone();
        tampered.as_object_mut().unwrap().insert(
            "agent_id".into(), serde_json::Value::String("EvilAgent".into()),
        );
        assert!(!verify_body(&id.fleet_token, &tampered, &env.hmac));
    }

    #[test]
    fn verify_rejects_wrong_token() {
        let id = ident(Role::Multiply);
        let env = build_join_request(&id, vec![], "h".into(), "0.1.0".into(), 100);
        let wrong_token = [42u8; 32];
        assert!(!verify_body(&wrong_token, &env.body, &env.hmac));
    }

    #[test]
    fn verify_rejects_garbage_hmac() {
        let id = ident(Role::Multiply);
        let env = build_join_request(&id, vec![], "h".into(), "0.1.0".into(), 100);
        assert!(!verify_body(&id.fleet_token, &env.body, "not-hex"));
        assert!(!verify_body(&id.fleet_token, &env.body, "deadbeef"));
    }

    #[test]
    fn accept_join_paths_to_paired() {
        let id = ident(Role::Multiply);
        let env = build_accept_join(
            &id, "OrchestratorAgentId".into(), id.agent_id.clone(), 999,
        );
        let new = apply_accept_join(&id, &env, &PairingState::Unpaired).unwrap();
        assert!(matches!(&new, PairingState::Paired { orchestrator_agent_id, paired_at } if orchestrator_agent_id == "OrchestratorAgentId" && *paired_at == 999));
    }

    #[test]
    fn accept_join_rejects_wrong_worker() {
        let id = ident(Role::Multiply);
        let env = build_accept_join(
            &id, "Orch".into(), "OtherWorker".into(), 1,
        );
        let result = apply_accept_join(&id, &env, &PairingState::Unpaired);
        assert!(matches!(result, Err(PairingError::WrongWorker)));
    }

    #[test]
    fn accept_join_rejects_wrong_fleet() {
        let id = ident(Role::Multiply);
        let other = FleetIdentity {
            fleet_id: [99; 8],
            fleet_token: id.fleet_token,
            role: Role::Multiply,
            agent_id: id.agent_id.clone(),
        };
        let env = build_accept_join(&other, "Orch".into(), id.agent_id.clone(), 1);
        let result = apply_accept_join(&id, &env, &PairingState::Unpaired);
        assert!(matches!(result, Err(PairingError::WrongFleet)));
    }

    #[test]
    fn accept_join_blocks_silent_orchestrator_swap() {
        let id = ident(Role::Multiply);
        let env_a = build_accept_join(&id, "OrchA".into(), id.agent_id.clone(), 1);
        let s = apply_accept_join(&id, &env_a, &PairingState::Unpaired).unwrap();
        let env_b = build_accept_join(&id, "OrchB".into(), id.agent_id.clone(), 2);
        let result = apply_accept_join(&id, &env_b, &s);
        assert!(matches!(result, Err(PairingError::AlreadyPairedDifferent)));
    }

    #[test]
    fn accept_join_idempotent_with_same_orchestrator() {
        let id = ident(Role::Multiply);
        let env = build_accept_join(&id, "OrchA".into(), id.agent_id.clone(), 5);
        let s = apply_accept_join(&id, &env, &PairingState::Unpaired).unwrap();
        let s2 = apply_accept_join(&id, &env, &s).unwrap();
        assert_eq!(s, s2);
    }

    #[test]
    fn accept_join_rejects_bad_hmac() {
        let id = ident(Role::Multiply);
        let mut env = build_accept_join(&id, "Orch".into(), id.agent_id.clone(), 1);
        env.hmac = "00".repeat(32); // Wrong length not the issue, just wrong bytes
        let result = apply_accept_join(&id, &env, &PairingState::Unpaired);
        assert!(matches!(result, Err(PairingError::BadHmac)));
    }

    #[test]
    fn paired_state_filters_correctly() {
        let s = PairingState::Paired {
            orchestrator_agent_id: "OrchA".into(),
            paired_at: 1,
        };
        assert!(s.is_paired_with("OrchA"));
        assert!(!s.is_paired_with("OrchB"));
        assert!(!PairingState::Unpaired.is_paired_with("OrchA"));
    }

    #[test]
    fn role_parser_normalizes_variants() {
        assert_eq!(Role::from_str("multiply").unwrap(),    Role::Multiply);
        assert_eq!(Role::from_str("MULTIPLY").unwrap(),    Role::Multiply);
        assert_eq!(Role::from_str("hedged_jlp").unwrap(),  Role::HedgedJlp);
        assert_eq!(Role::from_str("hedged-jlp").unwrap(),  Role::HedgedJlp);
        assert_eq!(Role::from_str("riskWatcher").unwrap(), Role::RiskWatcher);
        assert!(Role::from_str("admin").is_err());
    }

    #[test]
    fn canonical_json_orders_keys() {
        let a = serde_json::json!({"z": 1, "a": 2});
        let b = serde_json::json!({"a": 2, "z": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }
}
