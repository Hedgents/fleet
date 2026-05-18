//! Per-custody USD pricing via Jupiter's lite Price API v3.
//!
//! ## Why HTTP instead of on-chain Pyth
//!
//! The JLP custody account body stores a `pythnet_price_account` pubkey
//! that points at the legacy V1 Pyth standalone oracle. As of 2025–2026
//! Pyth migrated to **Pull V2** ephemeral price-update PDAs: the legacy
//! pubkey no longer resolves on mainnet for several custodies. The live
//! daemon hit `AccountNotFound: pubkey=4KMnVxoujMMeEvfypaVFXARYuqVhjiCHUcBHbfdzJZKw`
//! on every `read_pool_state` tick (see hedgedjlp-live.log on the VPS),
//! which blocked the rebalancer-resize action shipped in
//! `fleet-v0.4.0-rc5` from ever firing.
//!
//! Switching to Jupiter's lite Price API:
//!  - matches what `tools/fleet-dashboard-server/src/chain/jlp_price.rs`
//!    already uses successfully (dashboard prices JLP this way today),
//!  - covers an arbitrary batch of mints in one HTTP call (cheap),
//!  - has no dependency on a particular oracle program version.
//!
//! ## Rate limits
//!
//! Jupiter's `lite-api.jup.ag` host allows a handful of requests/sec.
//! `read_pool_state` issues **one batched call per tick** which is well
//! inside that envelope. The `reqwest::Client` here carries a 10s
//! timeout so a hung Jupiter endpoint can't wedge the rebalancer.

use anyhow::{anyhow, Result};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, warn};

const JUPITER_PRICE_URL: &str = "https://lite-api.jup.ag/price/v3";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Fetch USD prices for a batch of SPL mints from Jupiter's lite Price
/// API. Returns a map: mint → micro-USD price (1e6 = $1.00).
///
/// One HTTP call covers all mints in the request — callers should batch.
/// Empty input returns an empty map without issuing a request.
///
/// On transport / status / shape failure the function returns an Err;
/// caller decides how to degrade (the daemon logs WARN + treats missing
/// prices as $0 contribution rather than failing the whole tick).
pub async fn fetch_custody_prices_micro_usd(
    mints: &[Pubkey],
) -> Result<HashMap<Pubkey, u128>> {
    if mints.is_empty() {
        return Ok(HashMap::new());
    }
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| anyhow!("build reqwest client: {e}"))?;
    let ids = mints
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let url = format!("{}?ids={}", JUPITER_PRICE_URL, ids);
    debug!(%url, "jupiter price api: requesting batch");
    let resp = client.get(&url).send().await?.error_for_status()?;
    let body: serde_json::Value = resp.json().await?;
    Ok(parse_price_response(&body, mints))
}

/// Parse a Jupiter `/price/v3` JSON body into a (mint → micro-USD) map.
///
/// Mints missing from the response or carrying a non-positive / non-
/// finite price are simply absent from the returned map; caller logs a
/// WARN for any expected mint that doesn't surface.
pub(crate) fn parse_price_response(
    body: &serde_json::Value,
    mints: &[Pubkey],
) -> HashMap<Pubkey, u128> {
    let mut out = HashMap::with_capacity(mints.len());
    for mint in mints {
        let key = mint.to_string();
        let usd_price = body
            .get(&key)
            .and_then(|v| v.get("usdPrice"))
            .and_then(|v| v.as_f64());
        let Some(usd) = usd_price else {
            warn!(%mint, "jupiter price api: missing usdPrice — treating as $0");
            continue;
        };
        if !usd.is_finite() || usd <= 0.0 {
            warn!(%mint, %usd, "jupiter price api: non-positive price — treating as $0");
            continue;
        }
        let micro = (usd * 1_000_000.0).round() as u128;
        out.insert(*mint, micro);
    }
    out
}

/// Convert `owned` (raw mint units at `decimals` decimals) into
/// micro-USD using a Jupiter-derived price (`micro-USD per 1 whole
/// token`).
///
/// Math: `whole_tokens = owned / 10^decimals`, `usd = whole × price_$`,
/// `micro_usd = usd × 1e6`. Combining:
///   `micro_usd = owned × price_micro_usd / 10^decimals`.
///
/// Computation in u128 to avoid overflow on large custodies (e.g. a
/// SOL custody can hold ~1M SOL = 1e15 raw lamports; × $200 micro
/// price ≈ 1e24 — fits in u128 but not u64). Clipped to u64 at the
/// end; saturating semantics mirror `scale_owned_to_micro_usd`.
pub(crate) fn scale_owned_to_micro_usd_from_jupiter(
    owned: u64,
    decimals: u8,
    price_micro_usd: u128,
) -> u64 {
    if price_micro_usd == 0 || owned == 0 {
        return 0;
    }
    let mantissa = (owned as u128).saturating_mul(price_micro_usd);
    let divisor = 10u128.pow(decimals as u32);
    let result = mantissa / divisor;
    result.min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_price_response_extracts_prices() {
        let sol: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
        let eth: Pubkey = solana_sdk::pubkey!("7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs");
        let body = json!({
            sol.to_string(): { "usdPrice": 200.5, "blockId": 1 },
            eth.to_string(): { "usdPrice": 3_000.0 },
        });
        let map = parse_price_response(&body, &[sol, eth]);
        assert_eq!(map.get(&sol).copied(), Some(200_500_000));
        assert_eq!(map.get(&eth).copied(), Some(3_000_000_000));
    }

    #[test]
    fn parse_price_response_skips_missing_mint() {
        let sol: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
        let body = json!({});
        let map = parse_price_response(&body, &[sol]);
        assert!(map.is_empty());
    }

    #[test]
    fn parse_price_response_skips_non_positive_price() {
        let sol: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
        let body = json!({ sol.to_string(): { "usdPrice": -5.0 } });
        assert!(parse_price_response(&body, &[sol]).is_empty());
        let body = json!({ sol.to_string(): { "usdPrice": 0.0 } });
        assert!(parse_price_response(&body, &[sol]).is_empty());
    }

    #[test]
    fn scale_owned_to_micro_usd_from_jupiter_sol_at_200() {
        // 1 SOL (9 decimals) × $200 = $200 = 200_000_000 micro-USD.
        let micro = scale_owned_to_micro_usd_from_jupiter(
            1_000_000_000, // 1 SOL raw
            9,
            200_000_000, // $200 in micro-USD
        );
        assert_eq!(micro, 200_000_000);
    }

    #[test]
    fn scale_owned_to_micro_usd_from_jupiter_btc_at_60k() {
        // 0.5 BTC at 8 decimals × $60_000 = $30_000 = 30_000_000_000 micro-USD.
        let owned: u64 = 50_000_000; // 0.5 BTC raw
        let micro = scale_owned_to_micro_usd_from_jupiter(owned, 8, 60_000_000_000);
        assert_eq!(micro, 30_000_000_000);
    }

    #[test]
    fn scale_owned_to_micro_usd_from_jupiter_zero_inputs() {
        assert_eq!(scale_owned_to_micro_usd_from_jupiter(0, 9, 200_000_000), 0);
        assert_eq!(scale_owned_to_micro_usd_from_jupiter(1_000_000_000, 9, 0), 0);
    }

    #[test]
    fn scale_owned_to_micro_usd_from_jupiter_large_sol_custody() {
        // 1M SOL custody × $200 = $200M = 2e14 micro-USD. Fits in u64.
        let owned: u64 = 1_000_000 * 1_000_000_000;
        let micro = scale_owned_to_micro_usd_from_jupiter(owned, 9, 200_000_000);
        assert_eq!(micro, 200_000_000_000_000);
    }

    #[tokio::test]
    async fn fetch_custody_prices_micro_usd_empty_input_no_request() {
        // Empty input must short-circuit without an HTTP call. If a
        // request were issued, this test would still pass (the call
        // would succeed or fail) — but the contract is documented:
        // empty → empty map, no IO.
        let map = fetch_custody_prices_micro_usd(&[]).await.expect("empty ok");
        assert!(map.is_empty());
    }
}
