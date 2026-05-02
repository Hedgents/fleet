use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::json;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;

use crate::handlers;
use crate::handlers::pyth::PythCache;
use crate::pairing::{FleetIdentity, PairingState};
use crate::persistence::StateFile;
use crate::rpc::RpcContext;
use crate::wallet::Wallet;

#[derive(Clone)]
pub struct AppState {
    pub rpc: RpcContext,
    pub wallet: Arc<Wallet>,
    /// `Some` when daemon was started with all four fleet flags.
    pub fleet_identity: Option<Arc<FleetIdentity>>,
    pub pairing: Arc<RwLock<PairingState>>,
    pub state_file: StateFile,
    pub pyth_cache: PythCache,
}

impl AppState {
    pub fn new(
        rpc: RpcContext,
        wallet: Wallet,
        fleet_identity: Option<FleetIdentity>,
        initial_pairing: PairingState,
        state_file: StateFile,
    ) -> Self {
        Self {
            rpc,
            wallet: Arc::new(wallet),
            fleet_identity: fleet_identity.map(Arc::new),
            pairing: Arc::new(RwLock::new(initial_pairing)),
            state_file,
            pyth_cache: PythCache::new(),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/identity", get(identity))
        // Fleet pairing
        .route("/fleet/status",                  get(handlers::fleet::status))
        .route("/fleet/pairing/join-request",    get(handlers::fleet::join_request))
        .route("/fleet/pairing/accept-join",     post(handlers::fleet::accept_join))
        .route("/fleet/pairing/revoke",          post(handlers::fleet::revoke))
        // DeFi protocols
        .route("/kamino/supply",   post(handlers::kamino::supply))
        .route("/kamino/withdraw", post(handlers::kamino::withdraw))
        // Pyth oracle (read-only)
        .route("/pyth/price/:symbol", get(handlers::pyth::price))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

async fn health() -> Response {
    Json(json!({"status": "ok"})).into_response()
}

async fn identity(State(state): State<AppState>) -> Response {
    Json(json!({"pubkey": state.wallet.pubkey().to_string()})).into_response()
}

// ── Shared error response shape ─────────────────────────────────────────────

#[derive(Serialize)]
pub struct ApiError {
    pub error: String,
}

pub fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(ApiError { error: msg.into() })).into_response()
}
