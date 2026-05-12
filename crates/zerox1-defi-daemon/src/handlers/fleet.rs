//! Fleet pairing HTTP endpoints.
//!
//! Exposes the pairing protocol over HTTP so any transport (libp2p mesh,
//! HTTP-bridge to zerox1-node, manual curl) can drive the flow without the
//! daemon needing libp2p as a direct dependency.
//!
//! Endpoints:
//!
//!   GET  /fleet/status                      → current pairing state, identity (no token)
//!   GET  /fleet/pairing/join-request        → freshly-built signed join-request envelope
//!   POST /fleet/pairing/accept-join         → apply incoming accept-join envelope
//!   POST /fleet/pairing/revoke              → clear paired orchestrator (manual recovery)

use std::time::{SystemTime, UNIX_EPOCH};

use axum::{extract::State, http::StatusCode, response::Response, Json};
use serde::Serialize;
use serde_json::json;

use crate::pairing::{apply_accept_join, build_join_request, PairingState, SignedEnvelope};
use crate::server::{err, AppState};

#[derive(Serialize)]
pub struct StatusResponse {
    pub fleet_id: String,
    pub agent_id: String,
    pub role: String,
    pub discovery_topic: String,
    pub state: PairingState,
}

pub async fn status(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    let id = match &state.fleet_identity {
        Some(i) => i,
        None => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "fleet identity not configured",
            )
        }
    };
    let pair_state = state.pairing.read().await.clone();
    Json(StatusResponse {
        fleet_id: hex::encode(id.fleet_id),
        agent_id: id.agent_id.clone(),
        role: format!("{:?}", id.role),
        discovery_topic: id.discovery_topic(),
        state: pair_state,
    })
    .into_response()
}

pub async fn join_request(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    let id = match &state.fleet_identity {
        Some(i) => i,
        None => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "fleet identity not configured",
            )
        }
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let envelope = build_join_request(
        id,
        capabilities_for_role(id.role),
        std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown-host".to_string()),
        env!("CARGO_PKG_VERSION").to_string(),
        now,
    );

    // Optimistic transition Unpaired → Pairing for visibility in /fleet/status.
    {
        let mut w = state.pairing.write().await;
        if matches!(*w, PairingState::Unpaired) {
            *w = PairingState::Pairing {
                sent_join_request_at: now,
            };
            let _ = state.state_file.save(&w);
        }
    }

    Json(envelope).into_response()
}

pub async fn accept_join(
    State(state): State<AppState>,
    Json(envelope): Json<SignedEnvelope>,
) -> Response {
    use axum::response::IntoResponse;
    let id = match &state.fleet_identity {
        Some(i) => i,
        None => {
            return err(
                StatusCode::SERVICE_UNAVAILABLE,
                "fleet identity not configured",
            )
        }
    };

    let mut w = state.pairing.write().await;
    let next = match apply_accept_join(id, &envelope, &w) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    *w = next.clone();
    if let Err(e) = state.state_file.save(&next) {
        tracing::error!(?e, "failed to persist new pairing state");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "persistence failed");
    }
    tracing::info!(?next, "pairing state updated");
    Json(json!({ "state": next })).into_response()
}

pub async fn revoke(State(state): State<AppState>) -> Response {
    use axum::response::IntoResponse;
    if state.fleet_identity.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "fleet identity not configured",
        );
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let new_state = PairingState::Revoked { revoked_at: now };
    {
        let mut w = state.pairing.write().await;
        *w = new_state.clone();
    }
    if let Err(e) = state.state_file.save(&new_state) {
        tracing::error!(?e, "failed to persist revoked state");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "persistence failed");
    }
    Json(json!({ "state": new_state })).into_response()
}

fn capabilities_for_role(role: crate::pairing::Role) -> Vec<String> {
    use crate::pairing::Role::*;
    match role {
        Multiply => vec![
            "kamino_supply".into(),
            "kamino_withdraw".into(),
            "kamino_multiply".into(),
        ],
        HedgedJlp => vec!["jlp_mint".into(), "jlp_burn".into(), "adrena_short".into()],
        StableFloor => vec![
            "kamino_supply".into(),
            "kamino_withdraw".into(),
            "sanctum_inf_stake".into(),
        ],
        RiskWatcher => vec!["read_positions".into(), "emergency_close".into()],
        Researcher => vec!["publish_brief".into()],
        Orchestrator => vec!["all".into()],
    }
}
