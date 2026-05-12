//! Live yield rates for all three Hedgents strategies.
//!
//! Sources:
//!   - Kamino API  `/reserves/metrics` → USDC supply+borrow, SOL borrow
//!   - Solana RPC  `getInflationRate`  → native staking base for jitoSOL
//!   - DeFiLlama   `/chart/<pool>`     → JLP fee APY proxy via Orca JLP-USDC
//!
//! All three are fetched in parallel; any failure falls back to 0 so
//! the daemons always emit a telemetry line (with 0 APR fields marked
//! as unavailable in logs).
//!
//! # Strategy math
//!
//! **stable-yield** (Kamino USDC supply):
//!   `net_apr = usdc_supply_bps`
//!
//! **multiply** (Kamino leveraged jitoSOL, target LTV 60 %):
//!   ```
//!   leverage   = 1 / (1 – 0.60) = 2.5×
//!   debt_ratio = leverage – 1   = 1.5×
//!   net_apr    = jitosol_apy × leverage  –  usdc_borrow × debt_ratio
//!   ```
//!   jitoSOL APY = Solana native staking APR + Jito MEV premium (~1.5%).
//!
//! **hedgedjlp** (JLP buy + delta-neutral Jupiter Perps short):
//!   ```
//!   net_apr = jlp_fee_apy  –  sol_borrow_rate × hedge_fraction (0.75)
//!   ```
//!   JLP fee APY from DeFiLlama Orca JLP-USDC pool (best public proxy).
//!   SOL borrow rate from Kamino (same order-of-magnitude as Perps borrow).

#[allow(unused_imports)]
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tracing::{info, warn};

// ── Kamino REST API ──────────────────────────────────────────────────────────
const KAMINO_MARKET: &str = "https://api.kamino.finance/kamino-market/\
     7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF/reserves/metrics";

// ── DeFiLlama: Orca JLP-USDC LP (best public proxy for JLP fee yield) ───────
// Pool uuid verified against DeFiLlama on 2026-05-09; highest-TVL JLP pool.
const ORCA_JLP_USDC_POOL: &str = "99306789-b083-4668-86da-4cedb1c9bfef";

// ── Jito MEV premium on top of native Solana staking ────────────────────────
// Jito's MEV auctions add approximately 1.5 % APR to staking returns.
// Verified against Jito's published stats (avg 2025–2026).
const JITO_MEV_PREMIUM_PCT: f64 = 1.5;

// ── Estimated stake participation rate ──────────────────────────────────────
// Fraction of total SOL supply that is actively staked. Historically 65–68%.
// Used to convert inflation rate → effective staking APR.
const STAKE_PARTICIPATION: f64 = 0.66;

// ── Multiply strategy parameters ────────────────────────────────────────────
const MULTIPLY_TARGET_LTV: f64 = 0.60;

// ── HedgedJLP hedge fraction ─────────────────────────────────────────────────
// We short ~75 % of JLP exposure to target δ ≈ 0.
const HEDGEDJLP_HEDGE_FRACTION: f64 = 0.75;

// ── Solana mainnet RPC (public) ───────────────────────────────────────────────
const SOLANA_RPC: &str = "https://api.mainnet-beta.solana.com";

// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FleetRates {
    // ── Raw inputs ────────────────────────────────────────────────────
    /// Kamino USDC main-market supply APR (%).
    pub kamino_usdc_supply_pct: f64,
    /// Kamino USDC main-market borrow APR (%).
    pub kamino_usdc_borrow_pct: f64,
    /// Kamino SOL borrow APR (%) — proxy for Jupiter Perps borrow cost.
    pub kamino_sol_borrow_pct: f64,
    /// jitoSOL effective APY (%) = Solana native APR + Jito MEV premium.
    pub jitosol_apy_pct: f64,
    /// JLP fee APY (%) from DeFiLlama Orca JLP-USDC pool.
    pub jlp_fee_apy_pct: f64,

    // ── Computed strategy net yields (basis points) ───────────────────
    /// stable-yield: direct Kamino USDC supply APR.
    pub stable_yield_apr_bps: u16,
    /// multiply: leveraged jitoSOL yield minus USDC borrow cost.
    pub multiply_net_apr_bps: u16,
    /// hedgedjlp: JLP fee minus hedge borrow cost.
    pub hedgedjlp_net_apr_bps: u16,
}

fn pct_to_bps(pct: f64) -> u16 {
    (pct * 100.0).round().clamp(0.0, u16::MAX as f64) as u16
}

impl FleetRates {
    fn compute(
        usdc_supply: f64,
        usdc_borrow: f64,
        sol_borrow: f64,
        jitosol_apy: f64,
        jlp_fee: f64,
    ) -> Self {
        let lev = 1.0 / (1.0 - MULTIPLY_TARGET_LTV);
        let debt = lev - 1.0;
        let multiply_net = (jitosol_apy * lev - usdc_borrow * debt).max(0.0);
        let hedge_cost = sol_borrow * HEDGEDJLP_HEDGE_FRACTION;
        let hedgedjlp_net = (jlp_fee - hedge_cost).max(0.0);

        info!(
            usdc_supply_pct = usdc_supply,
            usdc_borrow_pct = usdc_borrow,
            sol_borrow_pct = sol_borrow,
            jitosol_apy_pct = jitosol_apy,
            jlp_fee_apy_pct = jlp_fee,
            multiply_net_pct = multiply_net,
            hedgedjlp_net_pct = hedgedjlp_net,
            "fleet rates computed",
        );

        Self {
            kamino_usdc_supply_pct: usdc_supply,
            kamino_usdc_borrow_pct: usdc_borrow,
            kamino_sol_borrow_pct: sol_borrow,
            jitosol_apy_pct: jitosol_apy,
            jlp_fee_apy_pct: jlp_fee,
            stable_yield_apr_bps: pct_to_bps(usdc_supply),
            multiply_net_apr_bps: pct_to_bps(multiply_net),
            hedgedjlp_net_apr_bps: pct_to_bps(hedgedjlp_net),
        }
    }
}

/// Fetch all fleet strategy rates in parallel. Never panics — returns
/// a zeroed `FleetRates` on total failure so callers always emit telemetry.
pub async fn fetch_fleet_rates() -> FleetRates {
    let (kamino, jitosol, jlp) = tokio::join!(
        fetch_kamino_rates(),
        fetch_jitosol_apy(),
        fetch_jlp_fee_apy(),
    );
    let (usdc_supply, usdc_borrow, sol_borrow) = kamino;
    FleetRates::compute(usdc_supply, usdc_borrow, sol_borrow, jitosol, jlp)
}

// ── Kamino API ───────────────────────────────────────────────────────────────

/// Parse a JSON value that may be a number OR a numeric string.
/// The Kamino API returns rate fields as quoted strings.
fn parse_rate(v: Option<&serde_json::Value>) -> f64 {
    match v {
        None => 0.0,
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(serde_json::Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Returns (usdc_supply_pct, usdc_borrow_pct, sol_borrow_pct).
async fn fetch_kamino_rates() -> (f64, f64, f64) {
    match try_kamino_rates().await {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, "Kamino API fetch failed; using 0% for Kamino rates");
            (0.0, 0.0, 0.0)
        }
    }
}

async fn try_kamino_rates() -> anyhow::Result<(f64, f64, f64)> {
    // Use raw Value deserialization — resilient to schema changes in the API.
    // Set a browser UA — some APIs filter headless clients.
    let resp: Vec<serde_json::Value> = reqwest::Client::new()
        .get(KAMINO_MARKET)
        .header("User-Agent", "Mozilla/5.0 (compatible; Hedgents/1.0)")
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let n = resp.len();
    tracing::info!(reserve_count = n, "Kamino reserves fetched");
    if n == 0 {
        anyhow::bail!("Kamino API returned empty reserves array");
    }

    let mut usdc_supply = 0.0f64;
    let mut usdc_borrow = 0.0f64;
    let mut sol_borrow = 0.0f64;

    for r in &resp {
        // API field names: "liquidityToken", "supplyApy", "borrowApy" (fractions).
        let sym = r
            .get("liquidityToken")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();
        let s = parse_rate(r.get("supplyApy")) * 100.0;
        let b = parse_rate(r.get("borrowApy")) * 100.0;

        match sym.as_str() {
            "USDC" => {
                // Log raw values for debugging — remove once stable.
                let raw_s = r.get("supplyApy");
                let raw_b = r.get("borrowApy");
                tracing::info!(
                    usdc_supply_pct = s, usdc_borrow_pct = b,
                    raw_supply = ?raw_s, raw_borrow = ?raw_b,
                    "Kamino USDC rates"
                );
                usdc_supply = s;
                usdc_borrow = b;
            }
            "SOL" | "WSOL" => {
                tracing::info!(sol_borrow_pct = b, "Kamino SOL borrow rate");
                if b > sol_borrow {
                    sol_borrow = b;
                }
            }
            _ => {}
        }
    }

    Ok((usdc_supply, usdc_borrow, sol_borrow))
}

// ── jitoSOL APY via Solana RPC inflation rate ────────────────────────────────

#[derive(Deserialize)]
struct InflationRateResp {
    result: InflationRate,
}

#[derive(Deserialize)]
struct InflationRate {
    validator: f64,
}

/// jitoSOL APY = (validator_inflation / stake_participation) + Jito MEV.
async fn fetch_jitosol_apy() -> f64 {
    match try_jitosol_apy().await {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, "jitoSOL APY fetch failed; using 0%");
            0.0
        }
    }
}

async fn try_jitosol_apy() -> anyhow::Result<f64> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "getInflationRate",
        "params": []
    });
    let resp: InflationRateResp = reqwest::Client::new()
        .post(SOLANA_RPC)
        .json(&body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // validator fraction of total supply that goes to stakers, annualised.
    // Effective APR = what % of their stake they earn per year.
    let native_apy = (resp.result.validator / STAKE_PARTICIPATION) * 100.0;
    Ok(native_apy + JITO_MEV_PREMIUM_PCT)
}

// ── JLP fee APY via DeFiLlama ────────────────────────────────────────────────

#[derive(Deserialize)]
struct DlChartResp {
    data: Vec<DlEntry>,
}

#[derive(Deserialize)]
struct DlEntry {
    #[serde(rename = "apy", default)]
    apy: f64,
    #[serde(rename = "apyBase", default)]
    apy_base: f64,
    /// 7-day rolling average — preferred over the daily snapshot to avoid
    /// short-term volume spikes inflating paper P&L beyond sustainable rates.
    #[serde(rename = "apyBase7d", default)]
    apy_base_7d: f64,
}

async fn fetch_jlp_fee_apy() -> f64 {
    match try_jlp_fee_apy().await {
        Ok(v) => v,
        Err(e) => {
            warn!(?e, "JLP fee APY fetch failed; using 0%");
            0.0
        }
    }
}

async fn try_jlp_fee_apy() -> anyhow::Result<f64> {
    let url = format!("https://yields.llama.fi/chart/{}", ORCA_JLP_USDC_POOL);
    let resp: DlChartResp = reqwest::get(&url).await?.error_for_status()?.json().await?;

    let last = resp
        .data
        .last()
        .ok_or_else(|| anyhow::anyhow!("DeFiLlama JLP: empty data array"))?;

    // Prefer the 7-day rolling average to smooth out short-term volume spikes.
    // Fall back to the daily snapshot only when the 7d value is absent.
    let apy = if last.apy_base_7d > 0.0 {
        last.apy_base_7d
    } else if last.apy_base > 0.0 {
        last.apy_base
    } else {
        last.apy
    };
    Ok(apy)
}

// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rates(usdc_s: f64, usdc_b: f64, sol_b: f64, jitosol: f64, jlp: f64) -> FleetRates {
        FleetRates::compute(usdc_s, usdc_b, sol_b, jitosol, jlp)
    }

    #[test]
    fn stable_yield_is_usdc_supply() {
        let r = rates(3.72, 5.02, 7.58, 8.3, 30.0);
        assert_eq!(r.stable_yield_apr_bps, 372);
    }

    #[test]
    fn multiply_math_at_60pct_ltv() {
        // jitoSOL 8.3%, USDC borrow 5.02%, leverage 2.5×
        // net = 8.3×2.5 - 5.02×1.5 = 20.75 - 7.53 = 13.22% → 1322 bps
        let r = rates(3.72, 5.02, 7.58, 8.3, 30.0);
        let expected = ((8.3_f64 * 2.5 - 5.02 * 1.5) * 100.0).round() as u16;
        assert_eq!(r.multiply_net_apr_bps, expected);
    }

    #[test]
    fn hedgedjlp_math() {
        // JLP 30%, hedge cost 7.58%×0.75 = 5.685%, net = 24.315% → 2432 bps
        let r = rates(3.72, 5.02, 7.58, 8.3, 30.0);
        let expected = ((30.0_f64 - 7.58 * 0.75) * 100.0).round() as u16;
        assert_eq!(r.hedgedjlp_net_apr_bps, expected);
    }

    #[test]
    fn floor_prevents_negative_yields() {
        let r = rates(1.0, 50.0, 60.0, 2.0, 5.0);
        assert_eq!(r.multiply_net_apr_bps, 0);
        assert_eq!(r.hedgedjlp_net_apr_bps, 0);
    }
}
