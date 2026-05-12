//! Live Kamino USDC supply APR from DeFiLlama.
//!
//! Same endpoint the dashboard server uses so both surfaces agree.
//! Returns basis points (e.g. 401 = 4.01%). On any error returns 0
//! so callers can always emit a report without blocking.

const DEFILLAMA_CHART: &str = "https://yields.llama.fi/chart";
const KAMINO_MAIN_USDC_POOL_ID: &str = "d2141a59-c199-4be7-8d4b-c8223954836b";

/// Fetch the current Kamino USDC main-market supply APR in basis
/// points. Returns 0 on network error or unexpected response shape.
pub async fn fetch_kamino_usdc_apr_bps() -> u16 {
    match try_fetch().await {
        Ok(bps) => bps,
        Err(e) => {
            tracing::warn!(?e, "kamino APR fetch failed; reporting 0 bps");
            0
        }
    }
}

async fn try_fetch() -> anyhow::Result<u16> {
    let url = format!("{}/{}", DEFILLAMA_CHART, KAMINO_MAIN_USDC_POOL_ID);
    let body: serde_json::Value = reqwest::get(&url).await?.error_for_status()?.json().await?;

    let apy_pct = body
        .get("data")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.last())
        .and_then(|last| last.get("apy").or_else(|| last.get("apyBase")))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow::anyhow!("DeFiLlama: missing apy in last entry"))?;

    Ok((apy_pct * 100.0).round() as u16)
}
