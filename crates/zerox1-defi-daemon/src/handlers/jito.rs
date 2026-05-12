//! Jito stake-pool handler — `POST /jito/deposit-sol`.
//!
//! Direct path SOL → jitoSOL via the SPL stake-pool program. Used by the
//! Multiply Agent for the swap leg of an atomic leveraged deposit (no API
//! dependency, no DEX spread, single ix). Also usable standalone for
//! converting SOL to jitoSOL.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};

use zerox1_defi_protocols::protocols::jito::deposit_sol_ix;

use crate::handlers::kamino::ExecQuery;
use crate::rpc::classify_simulation;
use crate::server::{err, AppState};

const JITO_CU_LIMIT: u32 = 200_000;
const JITO_PRIORITY_FEE: u64 = 10_000;

#[derive(Deserialize)]
pub struct DepositSolRequest {
    /// Amount in raw lamports.
    pub amount: u64,
}

#[derive(Serialize)]
pub struct DepositSolResponse {
    pub txid: String,
    pub amount_lamports: u64,
    pub simulated: bool,
    pub layout_valid: Option<bool>,
    pub summary: Option<String>,
    pub logs: Option<Vec<String>>,
}

pub async fn deposit_sol(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<DepositSolRequest>,
) -> Response {
    use axum::response::IntoResponse;

    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }

    let user = state.wallet.pubkey();
    let ixs = match deposit_sol_ix(&user, &state.jito_pool, req.amount) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    if q.simulate {
        match state
            .rpc
            .build_sign_simulate(
                ixs,
                state.wallet.keypair(),
                JITO_CU_LIMIT,
                JITO_PRIORITY_FEE,
            )
            .await
        {
            Ok(sim) => {
                let (layout_valid, summary) = classify_simulation(&sim);
                let logs = sim
                    .logs
                    .map(|l| l.into_iter().rev().take(20).rev().collect());
                Json(DepositSolResponse {
                    txid: "<simulated>".to_string(),
                    amount_lamports: req.amount,
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
                JITO_CU_LIMIT,
                JITO_PRIORITY_FEE,
            )
            .await
        {
            Ok(sig) => Json(DepositSolResponse {
                txid: sig.to_string(),
                amount_lamports: req.amount,
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
