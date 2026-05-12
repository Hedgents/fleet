//! Pyth price reader endpoint.
//!
//! `GET /pyth/price/:symbol` → `{ price, conf, expo, pub_slot, age_seconds, conf_bps }`
//!
//! Read-only — no signing, no broadcast. Used by the future Risk Watcher to
//! detect LST depeg, stablecoin depeg, and large SOL moves.
//!
//! A tiny TTL cache (default 5s) sits in front of every feed to avoid
//! hammering Pyth when multiple agents poll concurrently.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use tokio::sync::RwLock;

use zerox1_defi_protocols::protocols::pyth::{decode_price, feed_for_symbol, PythPrice};

use crate::server::{err, AppState};

const CACHE_TTL: Duration = Duration::from_secs(5);

#[derive(Clone, Default)]
pub struct PythCache {
    inner: Arc<RwLock<HashMap<String, CachedPrice>>>,
}

#[derive(Clone)]
struct CachedPrice {
    fetched_at: Instant,
    price: PythPrice,
}

impl PythCache {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Serialize)]
pub struct PriceResponse {
    pub symbol: String,
    /// Floating-point price for human display.
    pub price: f64,
    /// Raw integer price (use with `expo`).
    pub price_raw: i64,
    pub conf: u64,
    pub expo: i32,
    /// Solana slot the receiver wrote this update at.
    pub posted_slot: u64,
    /// Unix seconds Pyth published this price.
    pub publish_time: i64,
    /// Seconds between this update's publish_time and now (server clock).
    pub age_seconds: u64,
    /// EMA price (smoothed; cross-check against `price` for spike attacks).
    pub ema_price: f64,
    /// Confidence interval in basis points of price.
    pub conf_bps: u32,
    /// Pyth feed identifier (32-byte hex).
    pub feed_id: String,
    /// Local-cache age (ms since this daemon last fetched the account).
    pub cache_age_ms: u128,
}

pub async fn price(State(state): State<AppState>, Path(symbol): Path<String>) -> Response {
    // Detect mainnet vs devnet by checking the configured RPC URL. We accept
    // either; the feed addresses differ so we have to know.
    let url = state.rpc.client.url();
    let devnet = url.contains("devnet");

    let symbol_upper = symbol.to_ascii_uppercase();
    let feed = match feed_for_symbol(&symbol_upper, devnet) {
        Some(f) => f,
        None => {
            let net = if devnet { "devnet" } else { "mainnet" };
            return err(
                StatusCode::NOT_FOUND,
                format!("no Pyth feed configured for {symbol_upper} on {net}"),
            );
        }
    };

    // Cache check
    let cache_key = format!(
        "{}:{}",
        if devnet { "devnet" } else { "mainnet" },
        symbol_upper
    );
    {
        let r = state.pyth_cache.inner.read().await;
        if let Some(c) = r.get(&cache_key) {
            if c.fetched_at.elapsed() < CACHE_TTL {
                return ok_response(&symbol_upper, &c.price, c.fetched_at);
            }
        }
    }

    // Cache miss — fetch from RPC
    let account = match state.rpc.client.get_account_data(&feed).await {
        Ok(d) => d,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("rpc fetch: {e}")),
    };
    let decoded = match decode_price(&account) {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("decode: {e}")),
    };

    let now = Instant::now();
    {
        let mut w = state.pyth_cache.inner.write().await;
        w.insert(
            cache_key,
            CachedPrice {
                fetched_at: now,
                price: decoded.clone(),
            },
        );
    }

    ok_response(&symbol_upper, &decoded, now)
}

fn ok_response(symbol: &str, p: &PythPrice, fetched_at: Instant) -> Response {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Json(PriceResponse {
        symbol: symbol.to_string(),
        price: p.as_f64(),
        price_raw: p.price,
        conf: p.conf,
        expo: p.expo,
        posted_slot: p.posted_slot,
        publish_time: p.publish_time,
        age_seconds: p.age_seconds(now_unix),
        ema_price: p.ema_as_f64(),
        conf_bps: p.conf_bps(),
        feed_id: hex::encode(p.feed_id),
        cache_age_ms: fetched_at.elapsed().as_millis(),
    })
    .into_response()
}
