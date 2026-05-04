//! Read-only position monitoring endpoints for the Risk Watcher agent.
//!
//! `GET /kamino/obligation`     — user's Kamino main-market obligation snapshot
//! `GET /jlp/balance`           — user's JLP token balance + USD value
//! `GET /adrena/position?side=short` — user's open Adrena position (long/short)
//! `GET /positions`             — aggregate snapshot of all three legs in one call
//!
//! All endpoints are pure-read against the configured RPC. They return raw
//! decoded fields plus convenience floats where useful, so the Risk Watcher
//! can apply its own thresholds without re-implementing the layout decoders.

use std::str::FromStr;

use axum::{extract::{Query, State}, http::StatusCode, response::Response, Json};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use zerox1_defi_protocols::{
    constants::KAMINO_MAIN_MARKET,
    protocols::{
        adrena::{derive_position, Side as AdrenaSide},
        kamino::derive_user_obligation,
    },
    util::ata,
};

use crate::adrena_loader::{fetch_position, DecodedPosition};
use crate::jlp_loader::{fetch_jlp_total_supply, fetch_pool_aum_usd, fetch_user_jlp_balance};
use crate::kamino_loader::{fetch_obligation, DecodedObligation, ObligationBorrow, ObligationDeposit};
use crate::server::{err, AppState};

/// Scaled-fraction divisor used by Kamino (60 fractional bits).
const SF_SHIFT: u32 = 60;

fn sf_to_f64(sf: u128) -> f64 {
    // 2^60 fits in f64 exactly; large sf values lose precision but are still
    // close enough for risk-watcher thresholds (we only need ~6 decimal sig figs).
    (sf as f64) / (1u128 << SF_SHIFT) as f64
}

/// Optional `?owner=<base58 pubkey>` query parameter — when present, the
/// endpoint queries that wallet's position; when absent, falls back to the
/// daemon's own wallet pubkey. Lets the Risk Watcher monitor the
/// orchestrator's positions without needing the orchestrator's keypair.
#[derive(Deserialize, Default)]
pub struct OwnerQuery {
    #[serde(default)]
    pub owner: Option<String>,
}

fn resolve_owner(q: &OwnerQuery, default: Pubkey) -> Result<Pubkey, String> {
    match &q.owner {
        Some(s) if !s.is_empty() => {
            Pubkey::from_str(s).map_err(|e| format!("invalid owner pubkey: {e}"))
        }
        _ => Ok(default),
    }
}

// ── /kamino/obligation ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct KaminoObligationDeposit {
    pub reserve: String,
    pub deposited_amount: u64,
    pub market_value_sf: u128,
    pub market_value_usd: f64,
}

#[derive(Serialize)]
pub struct KaminoObligationBorrow {
    pub reserve: String,
    pub borrowed_amount_sf: u128,
    pub market_value_sf: u128,
    pub market_value_usd: f64,
    pub borrow_factor_adjusted_market_value_sf: u128,
}

#[derive(Serialize)]
pub struct KaminoObligationSnapshot {
    pub address: String,
    pub lending_market: String,
    pub owner: String,
    pub deposits: Vec<KaminoObligationDeposit>,
    pub borrows: Vec<KaminoObligationBorrow>,
    pub deposited_value_usd: f64,
    pub borrow_factor_adjusted_debt_value_usd: f64,
    pub borrowed_assets_market_value_usd: f64,
    pub allowed_borrow_value_usd: f64,
    pub unhealthy_borrow_value_usd: f64,
    /// Health factor: `unhealthy_borrow_value / borrow_factor_adjusted_debt_value`.
    /// `>1.0` = healthy, `<=1.0` = liquidatable. `None` if no debt.
    pub health_factor: Option<f64>,
}

impl KaminoObligationSnapshot {
    fn from_decoded(d: DecodedObligation) -> Self {
        let bfa_debt = sf_to_f64(d.borrow_factor_adjusted_debt_value_sf);
        let unhealthy = sf_to_f64(d.unhealthy_borrow_value_sf);
        let health_factor = if bfa_debt > 0.0 {
            Some(unhealthy / bfa_debt)
        } else {
            None
        };
        Self {
            address: d.address.to_string(),
            lending_market: d.lending_market.to_string(),
            owner: d.owner.to_string(),
            deposits: d.deposits.into_iter().map(deposit_to_snapshot).collect(),
            borrows: d.borrows.into_iter().map(borrow_to_snapshot).collect(),
            deposited_value_usd: sf_to_f64(d.deposited_value_sf),
            borrow_factor_adjusted_debt_value_usd: bfa_debt,
            borrowed_assets_market_value_usd: sf_to_f64(d.borrowed_assets_market_value_sf),
            allowed_borrow_value_usd: sf_to_f64(d.allowed_borrow_value_sf),
            unhealthy_borrow_value_usd: unhealthy,
            health_factor,
        }
    }
}

fn deposit_to_snapshot(d: ObligationDeposit) -> KaminoObligationDeposit {
    KaminoObligationDeposit {
        reserve: d.reserve.to_string(),
        deposited_amount: d.deposited_amount,
        market_value_sf: d.market_value_sf,
        market_value_usd: sf_to_f64(d.market_value_sf),
    }
}

fn borrow_to_snapshot(b: ObligationBorrow) -> KaminoObligationBorrow {
    KaminoObligationBorrow {
        reserve: b.reserve.to_string(),
        borrowed_amount_sf: b.borrowed_amount_sf,
        market_value_sf: b.market_value_sf,
        market_value_usd: sf_to_f64(b.market_value_sf),
        borrow_factor_adjusted_market_value_sf: b.borrow_factor_adjusted_market_value_sf,
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum KaminoObligationResponse {
    Found(KaminoObligationSnapshot),
    /// User has no obligation initialized in the main market.
    NoObligation { address: String },
}

pub async fn kamino_obligation(
    State(state): State<AppState>,
    Query(q): Query<OwnerQuery>,
) -> Response {
    use axum::response::IntoResponse;
    let user = match resolve_owner(&q, state.wallet.pubkey()) {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let obligation = derive_user_obligation(&user, &KAMINO_MAIN_MARKET);
    match fetch_obligation(&state.rpc.client, &obligation).await {
        Ok(Some(decoded)) => Json(KaminoObligationResponse::Found(
            KaminoObligationSnapshot::from_decoded(decoded),
        ))
        .into_response(),
        Ok(None) => Json(KaminoObligationResponse::NoObligation {
            address: obligation.to_string(),
        })
        .into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("fetch obligation: {e}")),
    }
}

// ── /jlp/balance ────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct JlpBalanceSnapshot {
    pub user_ata: String,
    pub balance_raw: u64,
    pub balance_jlp: f64,
    pub jlp_total_supply_raw: u64,
    pub pool_aum_usd_raw: u128,
    pub pool_aum_usd: f64,
    /// JLP/USD spot price: aum_usd / total_supply, with both in their native scales.
    /// Pool AUM uses 6-decimal USD (USDC scale); JLP supply is 6-decimal.
    pub jlp_price_usd: f64,
    /// User's holding USD value at the spot price.
    pub balance_usd: f64,
}

pub async fn jlp_balance(
    State(state): State<AppState>,
    Query(q): Query<OwnerQuery>,
) -> Response {
    use axum::response::IntoResponse;
    let user = match resolve_owner(&q, state.wallet.pubkey()) {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let user_ata = ata(&user, &state.jlp_pool.jlp_mint);

    let bal_fut = fetch_user_jlp_balance(&state.rpc.client, &user_ata);
    let supply_fut = fetch_jlp_total_supply(&state.rpc.client);
    let aum_fut = fetch_pool_aum_usd(&state.rpc.client);

    let (bal_res, supply_res, aum_res) = tokio::join!(bal_fut, supply_fut, aum_fut);

    let (balance_raw, decimals) = match bal_res {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("balance: {e}")),
    };
    let supply = match supply_res {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("supply: {e}")),
    };
    let aum_raw = match aum_res {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("aum: {e}")),
    };

    let balance_jlp = balance_raw as f64 / 10f64.powi(decimals as i32);
    let pool_aum_usd = aum_raw as f64 / 1e6;
    let jlp_price_usd = if supply > 0 {
        pool_aum_usd / (supply as f64 / 1e6)
    } else {
        0.0
    };

    Json(JlpBalanceSnapshot {
        user_ata: user_ata.to_string(),
        balance_raw,
        balance_jlp,
        jlp_total_supply_raw: supply,
        pool_aum_usd_raw: aum_raw,
        pool_aum_usd,
        jlp_price_usd,
        balance_usd: balance_jlp * jlp_price_usd,
    })
    .into_response()
}

// ── /adrena/position ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AdrenaPositionQuery {
    /// "long" or "short". Defaults to "short" (the hedge direction).
    #[serde(default)]
    pub side: Option<String>,
    /// Optional override for the position owner (default: daemon's wallet).
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Serialize)]
pub struct AdrenaPositionSnapshot {
    pub address: String,
    pub owner: String,
    pub pool: String,
    pub custody: String,
    pub collateral_custody: String,
    pub side: &'static str,
    pub open_time: i64,
    pub update_time: i64,
    /// Raw `price` field from the Position account (10-decimal USD scale).
    /// `entry_price_usd = entry_price_raw / 1e10`. Verified against an active
    /// JitoSOL short opened on 2026-05-04: raw 840_300_000_000 → $84.03.
    pub entry_price_raw: u64,
    /// Convenience: `entry_price_raw / 1e10` (USD).
    pub entry_price_usd: f64,
    pub size_usd: f64,
    pub borrow_size_usd: f64,
    pub collateral_usd: f64,
    pub collateral_amount_raw: u64,
    pub unrealized_interest_usd: f64,
    /// Approximate effective leverage = size_usd / collateral_usd.
    pub leverage: f64,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AdrenaPositionResponse {
    Found(AdrenaPositionSnapshot),
    NoPosition { address: String, side: &'static str },
}

fn parse_side(s: Option<String>) -> Result<AdrenaSide, String> {
    match s.as_deref().unwrap_or("short").to_ascii_lowercase().as_str() {
        "long" => Ok(AdrenaSide::Long),
        "short" => Ok(AdrenaSide::Short),
        other => Err(format!("side must be 'long' or 'short', got {other}")),
    }
}

fn side_label(s: AdrenaSide) -> &'static str {
    match s {
        AdrenaSide::Long => "long",
        AdrenaSide::Short => "short",
    }
}

pub async fn adrena_position(
    State(state): State<AppState>,
    Query(q): Query<AdrenaPositionQuery>,
) -> Response {
    use axum::response::IntoResponse;

    let side = match parse_side(q.side) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let user = match resolve_owner(
        &OwnerQuery { owner: q.owner },
        state.wallet.pubkey(),
    ) {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let position_addr = derive_position(
        &user,
        &state.adrena_pool.pool,
        &state.adrena_pool.jitosol_custody.address,
        side,
    );

    match fetch_position(&state.rpc.client, &position_addr).await {
        Ok(Some(p)) => Json(AdrenaPositionResponse::Found(decoded_position_to_snapshot(p, side)))
            .into_response(),
        Ok(None) => Json(AdrenaPositionResponse::NoPosition {
            address: position_addr.to_string(),
            side: side_label(side),
        })
        .into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("fetch position: {e}")),
    }
}

fn decoded_position_to_snapshot(p: DecodedPosition, side: AdrenaSide) -> AdrenaPositionSnapshot {
    let collateral_usd = p.collateral_usd_e6 as f64 / 1e6;
    let size_usd = p.size_usd_e6 as f64 / 1e6;
    let leverage = if collateral_usd > 0.0 { size_usd / collateral_usd } else { 0.0 };
    AdrenaPositionSnapshot {
        address: p.address.to_string(),
        owner: p.owner.to_string(),
        pool: p.pool.to_string(),
        custody: p.custody.to_string(),
        collateral_custody: p.collateral_custody.to_string(),
        side: side_label(side),
        open_time: p.open_time,
        update_time: p.update_time,
        entry_price_raw: p.entry_price_usd_e6,
        entry_price_usd: p.entry_price_usd_e6 as f64 / 1e10,
        size_usd,
        borrow_size_usd: p.borrow_size_usd_e6 as f64 / 1e6,
        collateral_usd,
        collateral_amount_raw: p.collateral_amount,
        unrealized_interest_usd: p.unrealized_interest_usd_e6 as f64 / 1e6,
        leverage,
    }
}

// ── /positions (aggregate) ──────────────────────────────────────────────────

#[derive(Serialize)]
pub struct PositionsSnapshot {
    pub wallet: String,
    pub kamino: KaminoObligationResponse,
    pub jlp: JlpBalanceSnapshot,
    pub adrena_short: AdrenaPositionResponse,
}

pub async fn positions(
    State(state): State<AppState>,
    Query(q): Query<OwnerQuery>,
) -> Response {
    use axum::response::IntoResponse;
    let user = match resolve_owner(&q, state.wallet.pubkey()) {
        Ok(o) => o,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };

    // Issue all three reads concurrently.
    let kamino_obligation_addr = derive_user_obligation(&user, &KAMINO_MAIN_MARKET);
    let user_jlp_ata = ata(&user, &state.jlp_pool.jlp_mint);
    let adrena_position_addr = derive_position(
        &user,
        &state.adrena_pool.pool,
        &state.adrena_pool.jitosol_custody.address,
        AdrenaSide::Short,
    );

    let kamino_fut = fetch_obligation(&state.rpc.client, &kamino_obligation_addr);
    let jlp_balance_fut = fetch_user_jlp_balance(&state.rpc.client, &user_jlp_ata);
    let jlp_supply_fut = fetch_jlp_total_supply(&state.rpc.client);
    let jlp_aum_fut = fetch_pool_aum_usd(&state.rpc.client);
    let adrena_fut = fetch_position(&state.rpc.client, &adrena_position_addr);

    let (kamino_res, bal_res, supply_res, aum_res, adrena_res) =
        tokio::join!(kamino_fut, jlp_balance_fut, jlp_supply_fut, jlp_aum_fut, adrena_fut);

    let kamino = match kamino_res {
        Ok(Some(d)) => KaminoObligationResponse::Found(KaminoObligationSnapshot::from_decoded(d)),
        Ok(None) => KaminoObligationResponse::NoObligation {
            address: kamino_obligation_addr.to_string(),
        },
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("kamino obligation: {e}")),
    };

    let (balance_raw, decimals) = match bal_res {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jlp balance: {e}")),
    };
    let supply = match supply_res {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jlp supply: {e}")),
    };
    let aum_raw = match aum_res {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jlp aum: {e}")),
    };
    let balance_jlp = balance_raw as f64 / 10f64.powi(decimals as i32);
    let pool_aum_usd = aum_raw as f64 / 1e6;
    let jlp_price_usd = if supply > 0 {
        pool_aum_usd / (supply as f64 / 1e6)
    } else {
        0.0
    };
    let jlp = JlpBalanceSnapshot {
        user_ata: user_jlp_ata.to_string(),
        balance_raw,
        balance_jlp,
        jlp_total_supply_raw: supply,
        pool_aum_usd_raw: aum_raw,
        pool_aum_usd,
        jlp_price_usd,
        balance_usd: balance_jlp * jlp_price_usd,
    };

    let adrena_short = match adrena_res {
        Ok(Some(p)) => {
            AdrenaPositionResponse::Found(decoded_position_to_snapshot(p, AdrenaSide::Short))
        }
        Ok(None) => AdrenaPositionResponse::NoPosition {
            address: adrena_position_addr.to_string(),
            side: "short",
        },
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("adrena position: {e}")),
    };

    Json(PositionsSnapshot {
        wallet: user.to_string(),
        kamino,
        jlp,
        adrena_short,
    })
    .into_response()
}

// silence unused-import warning when only used by handlers
const _: fn(&Pubkey, &Pubkey) -> Pubkey = ata;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sf_to_f64_handles_one() {
        assert!((sf_to_f64(1u128 << 60) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn sf_to_f64_handles_typical_usd_value() {
        // 14_526.348 stored as 14526.348 * 2^60
        let usd = 14_526.348_391;
        let sf = (usd * (1u128 << 60) as f64) as u128;
        let recovered = sf_to_f64(sf);
        assert!((recovered - usd).abs() < 0.01, "got {recovered}, expected {usd}");
    }

    #[test]
    fn sf_to_f64_zero() {
        assert_eq!(sf_to_f64(0), 0.0);
    }

    #[test]
    fn parse_side_defaults_to_short() {
        assert_eq!(parse_side(None).unwrap(), AdrenaSide::Short);
    }

    #[test]
    fn parse_side_accepts_long_and_short() {
        assert_eq!(parse_side(Some("long".to_string())).unwrap(), AdrenaSide::Long);
        assert_eq!(parse_side(Some("LONG".to_string())).unwrap(), AdrenaSide::Long);
        assert_eq!(parse_side(Some("Short".to_string())).unwrap(), AdrenaSide::Short);
    }

    #[test]
    fn parse_side_rejects_invalid() {
        assert!(parse_side(Some("middle".to_string())).is_err());
    }

    #[test]
    fn resolve_owner_falls_back_to_default() {
        let default = Pubkey::new_unique();
        let q = OwnerQuery { owner: None };
        assert_eq!(resolve_owner(&q, default).unwrap(), default);
    }

    #[test]
    fn resolve_owner_uses_query_when_present() {
        let default = Pubkey::new_unique();
        let other = Pubkey::new_unique();
        let q = OwnerQuery { owner: Some(other.to_string()) };
        assert_eq!(resolve_owner(&q, default).unwrap(), other);
    }

    #[test]
    fn resolve_owner_rejects_invalid_pubkey() {
        let q = OwnerQuery { owner: Some("not-a-pubkey".to_string()) };
        assert!(resolve_owner(&q, Pubkey::default()).is_err());
    }

    #[test]
    fn resolve_owner_treats_empty_string_as_default() {
        let default = Pubkey::new_unique();
        let q = OwnerQuery { owner: Some(String::new()) };
        assert_eq!(resolve_owner(&q, default).unwrap(), default);
    }
}
