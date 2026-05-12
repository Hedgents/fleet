//! Adrena perps — open / close short position handlers.
//!
//! `POST /adrena/short`        — open a JitoSOL short with USDC collateral
//! `POST /adrena/close-short`  — close a JitoSOL short (full or partial)
//!
//! Both accept `?simulate=true`. Adrena positions are leveraged perp shorts:
//! the position consumes ~$collateral × leverage of effective notional and
//! pays continuous borrow fees while open.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};

use zerox1_defi_protocols::protocols::adrena::{
    add_collateral_short_ix, close_position_short_ix, open_position_short_ix,
    remove_collateral_short_ix,
};

use crate::handlers::kamino::ExecQuery;
use crate::rpc::classify_simulation;
use crate::server::{err, AppState};

// Adrena open/close fits comfortably in 600k CU (lots of writes + oracle ix).
const ADRENA_CU_LIMIT: u32 = 600_000;
const ADRENA_PRIORITY_FEE: u64 = 10_000;

// ── Request / Response shapes ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OpenShortRequest {
    /// USDC collateral in raw units (1 USDC = 1_000_000).
    pub collateral_usdc: u64,
    /// Leverage in basis points (10_000 = 1x, 20_000 = 2x). Adrena typically
    /// caps shorts at 50x = 500_000 bps; the program enforces.
    pub leverage_bps: u32,
    /// Optional max entry price in 6-decimal USD (e.g. 80_000_000 = $80).
    /// `None` = accept any price (sets the field to u64::MAX).
    #[serde(default)]
    pub max_entry_price_usd_e6: Option<u64>,
}

#[derive(Deserialize)]
pub struct CloseShortRequest {
    /// Percentage to close in basis points (10_000 = full close, 5_000 = half).
    pub percentage_bps: u64,
    /// Optional limit price in 6-decimal USD. `None` = market close.
    #[serde(default)]
    pub min_exit_price_usd_e6: Option<u64>,
}

#[derive(Serialize)]
pub struct AdrenaExecResponse {
    pub txid: String,
    pub direction: &'static str,
    pub collateral_usdc: Option<u64>,
    pub leverage_bps: Option<u32>,
    pub percentage_bps: Option<u64>,
    pub simulated: bool,
    pub layout_valid: Option<bool>,
    pub summary: Option<String>,
    pub logs: Option<Vec<String>>,
}

// ── Handlers ────────────────────────────────────────────────────────────────

pub async fn open_short(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<OpenShortRequest>,
) -> Response {
    if req.collateral_usdc == 0 {
        return err(StatusCode::BAD_REQUEST, "collateral_usdc must be > 0");
    }
    if req.leverage_bps == 0 {
        return err(StatusCode::BAD_REQUEST, "leverage_bps must be > 0");
    }

    let user = state.wallet.pubkey();
    let max_price = req.max_entry_price_usd_e6.unwrap_or(u64::MAX);

    let ixs = match open_position_short_ix(
        &user,
        &state.adrena_pool,
        req.collateral_usdc,
        req.leverage_bps,
        max_price,
    ) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    execute(
        &state,
        ixs,
        "open_short",
        Some(req.collateral_usdc),
        Some(req.leverage_bps),
        None,
        q.simulate,
    )
    .await
}

pub async fn close_short(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<CloseShortRequest>,
) -> Response {
    if req.percentage_bps == 0 || req.percentage_bps > 10_000 {
        return err(
            StatusCode::BAD_REQUEST,
            "percentage_bps must be in (0, 10_000]",
        );
    }

    let user = state.wallet.pubkey();
    let ixs = match close_position_short_ix(
        &user,
        &state.adrena_pool,
        req.percentage_bps,
        req.min_exit_price_usd_e6,
    ) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    execute(
        &state,
        ixs,
        "close_short",
        None,
        None,
        Some(req.percentage_bps),
        q.simulate,
    )
    .await
}

async fn execute(
    state: &AppState,
    ixs: Vec<solana_sdk::instruction::Instruction>,
    direction: &'static str,
    collateral_usdc: Option<u64>,
    leverage_bps: Option<u32>,
    percentage_bps: Option<u64>,
    simulate: bool,
) -> Response {
    use axum::response::IntoResponse;

    if simulate {
        match state
            .rpc
            .build_sign_simulate(
                ixs,
                state.wallet.keypair(),
                ADRENA_CU_LIMIT,
                ADRENA_PRIORITY_FEE,
            )
            .await
        {
            Ok(sim) => {
                let (layout_valid, summary) = classify_simulation(&sim);
                let logs = sim
                    .logs
                    .map(|l| l.into_iter().rev().take(20).rev().collect());
                Json(AdrenaExecResponse {
                    txid: "<simulated>".to_string(),
                    direction,
                    collateral_usdc,
                    leverage_bps,
                    percentage_bps,
                    simulated: true,
                    layout_valid: Some(layout_valid),
                    summary: Some(summary),
                    logs,
                })
                .into_response()
            }
            Err(e) => err(StatusCode::BAD_GATEWAY, format!("simulate: {e}")),
        }
    } else {
        match state
            .rpc
            .build_sign_send(
                ixs,
                state.wallet.keypair(),
                ADRENA_CU_LIMIT,
                ADRENA_PRIORITY_FEE,
            )
            .await
        {
            Ok(sig) => Json(AdrenaExecResponse {
                txid: sig.to_string(),
                direction,
                collateral_usdc,
                leverage_bps,
                percentage_bps,
                simulated: false,
                layout_valid: None,
                summary: None,
                logs: None,
            })
            .into_response(),
            Err(e) => err(StatusCode::BAD_GATEWAY, format!("broadcast: {e}")),
        }
    }
}

// ── /adrena/add-collateral & /adrena/remove-collateral ─────────────────────
//
// Resize the JitoSOL short's collateral cushion without changing position
// size. Add reduces effective leverage; remove increases it (program rejects
// if it would breach max leverage).
//
// Add takes raw USDC tokens; remove takes USD value in 6-decimal scale —
// quirk of the Adrena IDL (`AddCollateralShortParams.collateral` is
// token-units, `RemoveCollateralShortParams.collateralUsd` is USD-units).

#[derive(Deserialize)]
pub struct AddCollateralRequest {
    /// Raw USDC tokens to add (1 USDC = 1_000_000).
    pub collateral_usdc: u64,
}

#[derive(Deserialize)]
pub struct RemoveCollateralRequest {
    /// USD value to remove in 6-decimal scaling (e.g. 5_000_000 = $5).
    pub collateral_usd_e6: u64,
}

pub async fn add_collateral(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<AddCollateralRequest>,
) -> Response {
    if req.collateral_usdc == 0 {
        return err(StatusCode::BAD_REQUEST, "collateral_usdc must be > 0");
    }
    let user = state.wallet.pubkey();
    let ix = match add_collateral_short_ix(&user, &state.adrena_pool, req.collateral_usdc) {
        Ok(i) => i,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    execute(
        &state,
        vec![ix],
        "add_collateral_short",
        Some(req.collateral_usdc),
        None,
        None,
        q.simulate,
    )
    .await
}

pub async fn remove_collateral(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<RemoveCollateralRequest>,
) -> Response {
    if req.collateral_usd_e6 == 0 {
        return err(StatusCode::BAD_REQUEST, "collateral_usd_e6 must be > 0");
    }
    let user = state.wallet.pubkey();
    let ix = match remove_collateral_short_ix(&user, &state.adrena_pool, req.collateral_usd_e6) {
        Ok(i) => i,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    execute(
        &state,
        vec![ix],
        "remove_collateral_short",
        // Re-using collateral_usdc field for USD-e6 here — caller can read
        // direction tag for clarity.
        Some(req.collateral_usd_e6),
        None,
        None,
        q.simulate,
    )
    .await
}
