//! HTTP/WebSocket API. Lives at 127.0.0.1:7700 by default. No auth
//! (local-only by design — the dashboard is the operator's own laptop).

use std::sync::Arc;

use axum::Router;
use tokio::sync::broadcast;

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
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(events::router())
        .merge(state::router())
        .with_state(state)
}
