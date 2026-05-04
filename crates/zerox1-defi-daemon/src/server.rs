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

use zerox1_defi_protocols::protocols::adrena::PoolMeta as AdrenaPoolMeta;
use zerox1_defi_protocols::protocols::jito::StakePoolMeta as JitoStakePoolMeta;
use zerox1_defi_protocols::protocols::jlp::PoolMeta;
use zerox1_defi_protocols::protocols::kamino::ReserveAccounts;

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
    /// Kamino main-market USDC reserve metadata loaded from on-chain at startup.
    pub kamino_usdc_reserve: Arc<ReserveAccounts>,
    /// Kamino main-market SOL reserve (Multiply borrow leg).
    pub kamino_sol_reserve: Arc<ReserveAccounts>,
    /// Kamino main-market JitoSOL reserve (Multiply collateral leg).
    pub kamino_jitosol_reserve: Arc<ReserveAccounts>,
    /// JLP pool + 5 custodies loaded from on-chain at startup.
    pub jlp_pool: Arc<PoolMeta>,
    /// Adrena main-pool + JitoSOL/USDC custodies for SOL hedge shorts.
    pub adrena_pool: Arc<AdrenaPoolMeta>,
    /// Jito stake pool metadata for direct SOL → jitoSOL conversion (Multiply
    /// swap leg, no API dependency).
    pub jito_pool: Arc<JitoStakePoolMeta>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc: RpcContext,
        wallet: Wallet,
        fleet_identity: Option<FleetIdentity>,
        initial_pairing: PairingState,
        state_file: StateFile,
        kamino_usdc_reserve: ReserveAccounts,
        kamino_sol_reserve: ReserveAccounts,
        kamino_jitosol_reserve: ReserveAccounts,
        jlp_pool: PoolMeta,
        adrena_pool: AdrenaPoolMeta,
        jito_pool: JitoStakePoolMeta,
    ) -> Self {
        Self {
            rpc,
            wallet: Arc::new(wallet),
            fleet_identity: fleet_identity.map(Arc::new),
            pairing: Arc::new(RwLock::new(initial_pairing)),
            state_file,
            pyth_cache: PythCache::new(),
            kamino_usdc_reserve: Arc::new(kamino_usdc_reserve),
            kamino_sol_reserve: Arc::new(kamino_sol_reserve),
            kamino_jitosol_reserve: Arc::new(kamino_jitosol_reserve),
            jlp_pool: Arc::new(jlp_pool),
            adrena_pool: Arc::new(adrena_pool),
            jito_pool: Arc::new(jito_pool),
        }
    }

    /// Look up a Kamino reserve by liquidity-mint asset name.
    /// Supported: "usdc", "sol"/"wsol", "jitosol".
    pub fn kamino_reserve(&self, asset: &str) -> Option<Arc<ReserveAccounts>> {
        match asset.to_ascii_lowercase().as_str() {
            "usdc" => Some(self.kamino_usdc_reserve.clone()),
            "sol" | "wsol" => Some(self.kamino_sol_reserve.clone()),
            "jitosol" => Some(self.kamino_jitosol_reserve.clone()),
            _ => None,
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
        .route("/kamino/borrow",   post(handlers::kamino::borrow))
        .route("/kamino/repay",    post(handlers::kamino::repay))
        // Sanctum router (kept for back-compat; their public API was 502 as
        // of 2026-05-04 — use /swap/* instead which routes via Jupiter).
        .route("/sanctum/stake",   post(handlers::sanctum::stake))
        .route("/sanctum/unstake", post(handlers::sanctum::unstake))
        // Jupiter aggregator — the working swap path
        .route("/swap",                post(handlers::jupiter::swap))
        .route("/swap/sol-to-inf",     post(handlers::jupiter::stake_sol_to_inf))
        .route("/swap/inf-to-sol",     post(handlers::jupiter::unstake_inf_to_sol))
        // Jito stake pool — direct SOL → jitoSOL (Multiply swap leg)
        .route("/jito/deposit-sol",    post(handlers::jito::deposit_sol))
        // Multiply Agent — iterative leverage building
        .route("/multiply/lever-up",   post(handlers::multiply::lever_up))
        .route("/multiply/lever-down", post(handlers::multiply::lever_down))
        .route("/jlp/mint",        post(handlers::jlp::mint))
        .route("/jlp/burn",        post(handlers::jlp::burn))
        .route("/adrena/short",             post(handlers::adrena::open_short))
        .route("/adrena/close-short",       post(handlers::adrena::close_short))
        .route("/adrena/add-collateral",    post(handlers::adrena::add_collateral))
        .route("/adrena/remove-collateral", post(handlers::adrena::remove_collateral))
        // Risk Watcher — read-only position monitoring
        .route("/kamino/obligation",  get(handlers::positions::kamino_obligation))
        .route("/jlp/balance",        get(handlers::positions::jlp_balance))
        .route("/adrena/position",    get(handlers::positions::adrena_position))
        .route("/positions",          get(handlers::positions::positions))
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
