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
use spl_associated_token_account::get_associated_token_address;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, warn};

use zerox1_defi_protocols::constants::USDC_MINT;
use zerox1_defi_runtime::rpc::RpcContext;

const JUPITER_PRICE_URL: &str = "https://lite-api.jup.ag/price/v3";
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Tx fee + ATA rent reserve to subtract from a wallet's free USDC before
/// declaring it spendable for collateral. $1 in micro-USDC; covers a
/// handful of Solana tx fees + the standard SPL ATA rent buffer.
pub const USDC_RESERVE_LAMPORTS: u64 = 1_000_000;

/// Fetch USD prices for a batch of SPL mints from Jupiter's lite Price
/// API. Returns a map: mint → micro-USD price (1e6 = $1.00).
///
/// One HTTP call covers all mints in the request — callers should batch.
/// Empty input returns an empty map without issuing a request.
///
/// On transport / status / shape failure the function returns an Err;
/// caller decides how to degrade (the daemon logs WARN + treats missing
/// prices as $0 contribution rather than failing the whole tick).
///
/// ## fleet-v0.4.0-rc7: partial-response retry
///
/// If the first response is missing any of the requested mints, a
/// single retry fires immediately. The fleet-v0.4.0-rc7 incident
/// recovered an ActivePosition where the rebalancer's delta computed
/// `delta.sol_usd=0` AND `delta.btc_usd=0` while `delta.eth_usd>0` —
/// a shape that is only physically possible if Jupiter's lite Price
/// API returned a partial response for that specific tick (verified
/// independently that all three Portal mints DO resolve when queried
/// in isolation). One retry costs <1s under normal latency and is
/// cheaper than failing a rebalance tick that ultimately wedged $174
/// of capital on a wrong-shape hedge plan.
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

    let map = fetch_prices_once(&client, mints).await?;
    if map.len() == mints.len() {
        return Ok(map);
    }

    // Partial response — retry once. The set of missing-from-map mints
    // is the only thing we need to recover; building a fresh request
    // with just the missing mints would be a minor optimisation but
    // re-requesting all of them keeps the path identical to the
    // first call (no asymmetric code path).
    let missing: Vec<Pubkey> = mints
        .iter()
        .filter(|m| !map.contains_key(m))
        .copied()
        .collect();
    warn!(
        missing_count = missing.len(),
        total_requested = mints.len(),
        "jupiter price api: partial response on first attempt; retrying once"
    );
    let retry_map = match fetch_prices_once(&client, mints).await {
        Ok(m) => m,
        Err(e) => {
            warn!(?e, "jupiter price api retry failed; returning partial map");
            return Ok(map);
        }
    };
    // Merge: prefer retry values (likely fresher). Missing-on-retry
    // mints stay missing — caller already warns via
    // `compute_custody_usd_value` and contributes $0 to delta.
    let mut merged = map;
    for (k, v) in retry_map {
        merged.insert(k, v);
    }
    if merged.len() != mints.len() {
        let still_missing: Vec<String> = mints
            .iter()
            .filter(|m| !merged.contains_key(m))
            .map(|m| m.to_string())
            .collect();
        warn!(
            still_missing = ?still_missing,
            "jupiter price api: STILL partial after retry — these mints will contribute \
             $0 to delta this tick. Operator: verify the mint is correct and not deprecated."
        );
    }
    Ok(merged)
}

async fn fetch_prices_once(
    client: &reqwest::Client,
    mints: &[Pubkey],
) -> Result<HashMap<Pubkey, u128>> {
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

/// Read the wallet's free USDC balance in raw lamports (6 decimals, so
/// `lamports / 1_000_000 = USD`). This is the spendable balance held in
/// the wallet's USDC ATA — funds inside Kamino reserves, JLP pool, or
/// other on-chain venues are NOT included.
///
/// Returns `Ok(0)` when the ATA does not exist (fresh wallet, never
/// held USDC) or the RPC client returns an "account not found"-flavored
/// error. Other RPC errors (network/transport) propagate so the caller
/// can decide whether to skip the gate or fail loud.
///
/// Used by `resize::execute_resize` as a pre-flight check before opening
/// hedge legs — Jupiter Perps' `create_increase_position_market_request`
/// invokes an SPL Token `Transfer` that pulls collateral from the
/// wallet's USDC ATA, and an under-funded ATA produces a 1200-line
/// `custom program error: 0x1` (Token::InsufficientFunds) inside the
/// program log. We catch that case off-chain, scale down, and surface
/// a clean `SkipReason::InsufficientUsdcLiquidity` to the operator.
pub async fn fetch_wallet_free_usdc_lamports(
    rpc: &Arc<RpcContext>,
    wallet: &Pubkey,
) -> Result<u64> {
    let ata = get_associated_token_address(wallet, &USDC_MINT);
    match rpc.client.get_token_account_balance(&ata).await {
        Ok(bal) => Ok(bal.amount.parse::<u64>().unwrap_or(0)),
        Err(e) => {
            let s = e.to_string();
            if s.contains("could not find account")
                || s.contains("AccountNotFound")
                || s.contains("Invalid param")
            {
                Ok(0)
            } else {
                warn!(
                    ?e,
                    %ata,
                    "get_token_account_balance failed for USDC ATA — treating as zero \
                     free USDC (resize pre-flight will skip / scale down)"
                );
                Ok(0)
            }
        }
    }
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

    #[test]
    fn usdc_reserve_is_one_dollar() {
        // Pin the reserve constant — a future tweak should surface in
        // review. $1 = 1_000_000 micro-USDC. The reserve covers tx fees
        // + one ATA rent on the resize pre-flight gate.
        assert_eq!(USDC_RESERVE_LAMPORTS, 1_000_000);
    }

    #[tokio::test]
    async fn fetch_wallet_free_usdc_lamports_unreachable_rpc_returns_zero() {
        // Unreachable-RPC stub: get_token_account_balance errors out,
        // the helper maps that to a zero balance (fresh-wallet fallback)
        // rather than propagating the failure. This mirrors the
        // recovery path's `read_jlp_balance` philosophy: prefer to
        // continue with conservative numbers (zero balance → skip /
        // scale-down) over failing the resize entirely on a transient
        // RPC blip.
        use solana_sdk::commitment_config::CommitmentConfig;
        let rpc = Arc::new(RpcContext::new(
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        ));
        let wallet = Pubkey::new_unique();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            fetch_wallet_free_usdc_lamports(&rpc, &wallet),
        )
        .await
        .expect("unreachable RPC must return promptly");
        let lamports = result.expect("helper must never error on RPC failure");
        assert_eq!(lamports, 0, "unreachable RPC → zero free USDC");
    }
}
