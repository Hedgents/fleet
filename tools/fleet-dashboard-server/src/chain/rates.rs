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

// DeFiLlama yields API — stable public endpoint, no API key required.
// Pool ID is the Kamino lend main-market USDC pool on Solana.
const DEFILLAMA_CHART: &str = "https://yields.llama.fi/chart";
const KAMINO_MAIN_USDC_POOL_ID: &str = "d2141a59-c199-4be7-8d4b-c8223954836b";

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
    // DeFiLlama chart endpoint returns time-series for a specific pool.
    // The last entry has the current APY as a percentage (e.g. 3.70 = 3.70%).
    let url = format!("{}/{}", DEFILLAMA_CHART, KAMINO_MAIN_USDC_POOL_ID);
    let resp = reqwest::get(&url).await?.error_for_status()?;
    let body: Value = resp.json().await?;

    let data = body
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("DeFiLlama: missing 'data' array"))?;

    let last = data
        .last()
        .ok_or_else(|| anyhow::anyhow!("DeFiLlama: empty data array"))?;

    let apy_pct = last
        .get("apy")
        .or_else(|| last.get("apyBase"))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow::anyhow!("DeFiLlama: no apy field in last entry"))?;

    // DeFiLlama returns APY as a percentage (3.70 = 3.70%), convert to bps.
    Ok((apy_pct * 100.0).round() as u32)
}
