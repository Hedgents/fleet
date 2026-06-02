//! SOL/USD price fetcher via Jupiter Lite Price API v3.
//!
//! Mirrors `jlp_price.rs` — same endpoint, same response shape, same
//! micro-USD output scale. Cached one layer up in `chain::mod.rs` via
//! the 30s `ChainCache`. On any error the caller falls back to 0,
//! preserving the prior "SOL not counted in AUM" behaviour (the v0.4.12
//! fix surfaces idle SOL only when the price is available; an error
//! does NOT silently undercount AUM, it just skips the SOL component
//! and the next 30s cache cycle retries).
//!
//! Added in v0.4.12 to fix the AUM-undercount: idle wallet SOL was
//! previously invisible to the dashboard (`read_chain_aum_breakdown`
//! only valued the USDC residual). Today's leverage walk exposed the
//! gap when 0.62 SOL of recovered capital became visible only after
//! it landed inside the multiply obligation.

use anyhow::{anyhow, Result};
use serde_json::Value;

const JUPITER_PRICE_URL: &str = "https://lite-api.jup.ag/price/v3";
/// Native SOL token mint (wrapped form). Jupiter's Price API accepts
/// this and returns USD price.
const SOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";

/// Fetch the current USD price of 1 SOL from Jupiter's lite Price API,
/// expressed in micro-USD (1e-6 USD). Returns an error on transport,
/// status, or JSON-shape failure.
pub async fn fetch_sol_price_micro_usd() -> Result<u128> {
    let url = format!("{}?ids={}", JUPITER_PRICE_URL, SOL_MINT_STR);
    let resp = reqwest::get(&url).await?.error_for_status()?;
    let body: Value = resp.json().await?;
    let usd_price = body
        .get(SOL_MINT_STR)
        .and_then(|v| v.get("usdPrice"))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow!("jupiter price api: missing usdPrice for SOL"))?;
    if !usd_price.is_finite() || usd_price <= 0.0 {
        return Err(anyhow!("jupiter price api: non-positive SOL price"));
    }
    Ok((usd_price * 1_000_000.0).round() as u128)
}

/// Convert SOL lamports (9 decimals) at a given price (micro-USD per
/// whole SOL) to a USD value (micro-USD). Pure helper for unit tests.
pub fn value_micro_usd(sol_lamports: u64, price_micro_usd: u128) -> u64 {
    // lamports = 1e-9 SOL → (lamports × price_micro_per_sol) / 1e9
    let scaled = (sol_lamports as u128).saturating_mul(price_micro_usd) / 1_000_000_000;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_micro_usd_prices_demo_balance() {
        // 620_000_000 lamports = 0.62 SOL × $82 per SOL = $50.84
        let v = value_micro_usd(620_000_000, 82_000_000);
        // 620_000_000 × 82_000_000 / 1e9 = 50_840_000 micro-USD.
        assert_eq!(v, 50_840_000);
    }

    #[test]
    fn value_micro_usd_handles_zero_balance() {
        assert_eq!(value_micro_usd(0, 82_000_000), 0);
    }

    #[test]
    fn value_micro_usd_handles_zero_price() {
        // Defensive: never panic / overflow when the price feed errors.
        assert_eq!(value_micro_usd(620_000_000, 0), 0);
    }
}
