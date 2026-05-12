//! HTTP/WebSocket API. Lives at 127.0.0.1:7700 by default. No auth
//! (local-only by design — the dashboard is the operator's own laptop).

use std::sync::Arc;

use axum::http::Method;
use axum::Router;
use tokio::sync::broadcast;
use tower_http::cors::CorsLayer;

use crate::chain::ChainReader;
use crate::store::Store;
use crate::types::MeshEvent;

pub mod events;
pub mod state;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub chain: Arc<ChainReader>,
    pub event_broadcast: broadcast::Sender<MeshEvent>,
    pub wallet_pubkey: solana_sdk::pubkey::Pubkey,
    /// RPC URL the chain reader was constructed with. Exposed to the
    /// dashboard so it can show the operator which network they're on
    /// (devnet/mainnet) without re-parsing the same string from CLI.
    pub rpc_url: String,
}

pub fn router(state: AppState) -> Router {
    // CORS — the dashboard is read-only telemetry. Operator can bind to
    // 127.0.0.1 for SSH-tunnel-only access, or expose via Cloudflare
    // Tunnel for institutional viewers. Either way, any browser that
    // reaches the API is fetching the same public-by-design data, so
    // Any-origin is acceptable.
    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::OPTIONS])
        .allow_headers(tower_http::cors::Any)
        .allow_origin(tower_http::cors::Any);

    Router::new()
        .merge(events::router())
        .merge(state::router())
        .with_state(state)
        .layer(cors)
}
