//! Yield benchmark rates for the dashboard comparison card.
//!
//! Kamino USDC supply APR is fetched live from Kamino's public REST API
//! and cached 5 minutes. USDY and EFFR are static reference values that
//! change infrequently (T-bill auctions / FOMC meetings); they are baked
//! in with an explicit "as-of" date so the operator knows when to update.

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use tracing::warn;

const KAMINO_API: &str = "https://api.kamino.finance";
const KAMINO_MAIN_MARKET_STR: &str = "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF";
const KAMINO_MAIN_USDC_RESERVE_STR: &str = "D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59";

/// Static USDY APY — Ondo's yield-bearing USDC on Solana, backed by US
/// T-bills. Tracks the 4-week T-bill rate closely. Update when Ondo
/// publishes a new distribution rate.
const USDY_APY_BPS: u32 = 490; // 4.90% as of May 2026

/// Effective Federal Funds Rate mid-point as of the latest FOMC decision.
/// Target range 4.25–4.50% → mid 4.375%; round to 4.33% for display.
/// Update after each FOMC meeting.
const EFFR_BPS: u32 = 433; // 4.33% as of May 2026

#[derive(Debug, Clone, Serialize)]
pub struct RateSnapshot {
    /// Kamino USDC main-market supply APR, basis points. 0 when unavailable
    /// (devnet, API timeout, etc.).
    pub kamino_usdc_supply_bps: u32,
    /// Ondo USDY APY on Solana, basis points. Static reference rate.
    pub usdy_apy_bps: u32,
    /// Effective Federal Funds Rate, basis points. Static reference rate.
    pub effr_bps: u32,
    /// Unix timestamp when Kamino APR was last fetched. 0 if never.
    pub kamino_fetched_at: u64,
    /// Human note for the UI ("live" vs "unavailable (devnet)").
    pub kamino_note: &'static str,
}

impl Default for RateSnapshot {
    fn default() -> Self {
        Self {
            kamino_usdc_supply_bps: 0,
            usdy_apy_bps: USDY_APY_BPS,
            effr_bps: EFFR_BPS,
            kamino_fetched_at: 0,
            kamino_note: "loading",
        }
    }
}

/// Fetch fresh Kamino USDC supply APR from Kamino's public REST API.
/// Returns `(bps, note)`. On any error returns `(0, "unavailable")`.
pub async fn fetch_kamino_usdc_apy() -> (u32, &'static str) {
    match try_fetch_kamino().await {
        Ok(bps) => (bps, "live"),
        Err(e) => {
            warn!(?e, "kamino rate fetch failed; using 0");
            (0, "unavailable")
        }
    }
}

async fn try_fetch_kamino() -> Result<u32> {
    let url = format!(
        "{}/v1/lend/market/{}/reserves",
        KAMINO_API, KAMINO_MAIN_MARKET_STR
    );
    let resp = reqwest::get(&url).await?.error_for_status()?;
    let body: Value = resp.json().await?;

    // Kamino returns either a top-level array or { reserves: [...] }.
    let reserves = if let Some(arr) = body.as_array() {
        arr.clone()
    } else if let Some(arr) = body.get("reserves").and_then(|v| v.as_array()) {
        arr.clone()
    } else {
        anyhow::bail!("unexpected Kamino API shape: {body}");
    };

    for reserve in &reserves {
        if !is_usdc_reserve(reserve) {
            continue;
        }
        if let Some(bps) = extract_supply_bps(reserve) {
            return Ok(bps);
        }
    }
    anyhow::bail!("USDC reserve not found in Kamino API response")
}

fn is_usdc_reserve(r: &Value) -> bool {
    // Match by reserve address or mint address.
    for field in ["address", "reserve", "mintAddress", "liquidityMint", "tokenMint"] {
        if let Some(s) = r.get(field).and_then(|v| v.as_str()) {
            if s == KAMINO_MAIN_USDC_RESERVE_STR {
                return true;
            }
        }
    }
    // Fallback: match by symbol.
    if let Some(sym) = r.get("symbol").and_then(|v| v.as_str()) {
        if sym.eq_ignore_ascii_case("USDC") {
            return true;
        }
    }
    false
}

fn extract_supply_bps(r: &Value) -> Option<u32> {
    // Try known field paths, outermost first.
    let candidates: &[&[&str]] = &[
        &["supplyInterestAPY"],
        &["stats", "supplyInterestAPY"],
        &["supplyApy"],
        &["lendApy"],
        &["stats", "supplyApy"],
        &["apy"],
    ];
    for path in candidates {
        let mut cur = r;
        let mut matched = true;
        for key in *path {
            if let Some(next) = cur.get(key) {
                cur = next;
            } else {
                matched = false;
                break;
            }
        }
        if !matched {
            continue;
        }
        if let Some(f) = cur.as_f64() {
            // Kamino returns decimals in [0, 1] range (0.087 = 8.7%).
            let bps = if f > 1.0 {
                // Already a percentage like 8.7 — convert to bps.
                (f * 100.0).round() as u32
            } else {
                (f * 10_000.0).round() as u32
            };
            return Some(bps);
        }
    }
    None
}
