//! JLP price fetcher via Jupiter Lite Price API v3.
//!
//! The Jupiter Price API returns USD price for SPL token mints. We use it
//! to price the operator's JLP holdings on the dashboard. The result is
//! a USD price scaled by 1e6 (micro-USD) so it composes cleanly with the
//! existing 6-decimal JLP lamport scale: `value_micro_usd = (jlp_lamports
//! × price_micro_usd) / 1_000_000`.
//!
//! Caching is handled one layer up (in `chain::mod.rs` via the 30s
//! `ChainCache`). On any error the caller falls back to 0, preserving
//! the prior behaviour.

use anyhow::{anyhow, Result};
use serde_json::Value;

const JUPITER_PRICE_URL: &str = "https://lite-api.jup.ag/price/v3";
const JLP_MINT_STR: &str = "27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4";

/// Fetch the current USD price of 1 JLP from Jupiter's lite Price API,
/// expressed in micro-USD (1e-6 USD). Returns an error on transport,
/// status, or JSON-shape failure.
pub async fn fetch_jlp_price_micro_usd() -> Result<u128> {
    let url = format!("{}?ids={}", JUPITER_PRICE_URL, JLP_MINT_STR);
    let resp = reqwest::get(&url).await?.error_for_status()?;
    let body: Value = resp.json().await?;
    let usd_price = body
        .get(JLP_MINT_STR)
        .and_then(|v| v.get("usdPrice"))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow!("jupiter price api: missing usdPrice for JLP"))?;
    if !usd_price.is_finite() || usd_price <= 0.0 {
        return Err(anyhow!("jupiter price api: non-positive JLP price"));
    }
    Ok((usd_price * 1_000_000.0).round() as u128)
}

/// Multiply JLP balance (6-decimal lamports) by price (micro-USD per JLP)
/// to obtain the dollar value in micro-USD. Pure helper for unit tests.
pub fn value_micro_usd(jlp_balance_lamports: u64, price_micro_usd: u128) -> u64 {
    let scaled = (jlp_balance_lamports as u128).saturating_mul(price_micro_usd) / 1_000_000;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 45_165_980 lamports × $4 per JLP = $180.66.
    #[test]
    fn value_micro_usd_prices_demo_balance() {
        let v = value_micro_usd(45_165_980, 4_000_000);
        // 45_165_980 × 4 = 180_663_920 micro-USD = $180.66392.
        assert_eq!(v, 180_663_920);
    }

    #[test]
    fn value_micro_usd_handles_zero_balance() {
        assert_eq!(value_micro_usd(0, 4_000_000), 0);
    }

    #[test]
    fn value_micro_usd_handles_zero_price() {
        assert_eq!(value_micro_usd(1_000_000, 0), 0);
    }
}
