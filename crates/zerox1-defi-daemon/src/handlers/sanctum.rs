//! Sanctum-shaped INF stake / unstake handlers.
//!
//! ## Routing change (2026-05-04)
//!
//! These endpoints originally pointed at Sanctum's S-Pool router
//! (`sanctum-s-api.fly.dev/v1/swap/*`). That router has been returning 502
//! / refusing connections for an extended period. Rather than break callers
//! that already integrated with `/sanctum/{stake,unstake}`, the
//! implementation now routes through Jupiter under the hood — Jupiter
//! itself routes through Sanctum's S-Pool when that's the best price, so
//! economics are similar (~10-15bps spread for the 2-hop SOL→INF route).
//!
//! New code should prefer `/swap/sol-to-inf` and `/swap/inf-to-sol` directly
//! since they expose Jupiter's slippage knob; these legacy endpoints lock
//! slippage to 50 bps.

use axum::{extract::{Query, State}, http::StatusCode, response::Response, Json};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use solana_sdk::transaction::VersionedTransaction;

use zerox1_defi_protocols::{
    constants::{INF_MINT, WSOL_MINT},
    protocols::{
        jupiter::{
            quote_summary, SwapBuildParams, SwapBuildResp, SwapQuote, JUPITER_LITE_API,
        },
        sanctum::SwapSrc,
    },
};

use crate::handlers::kamino::ExecQuery;
use crate::rpc::classify_simulation;
use crate::server::{err, AppState};

const LEGACY_SLIPPAGE_BPS: u16 = 50;

#[derive(Deserialize)]
pub struct StakeRequest {
    /// Amount in raw lamports (1 SOL = 1_000_000_000).
    pub amount: u64,
}

#[derive(Deserialize)]
pub struct UnstakeRequest {
    /// Amount in raw INF units (INF has 9 decimals).
    pub amount: u64,
}

/// Response shape preserved for back-compat with callers that integrated
/// against the old Sanctum-router-backed endpoints. `swap_src` is always
/// reported as `Jup` since Jupiter is the actual aggregator now; `fee_amount`
/// and `fee_pct` are derived from the Jupiter quote (set to zero / "0" when
/// Jupiter doesn't expose a single fee figure for multi-hop routes).
#[derive(Serialize)]
pub struct SanctumExecResponse {
    pub txid: String,
    pub direction: &'static str,
    pub amount_in: u64,
    pub amount_out: u64,
    pub fee_amount: u64,
    pub fee_pct: String,
    pub swap_src: SwapSrc,
    pub simulated: bool,
    pub layout_valid: Option<bool>,
    pub summary: Option<String>,
    pub logs: Option<Vec<String>>,
    /// Marker so callers can distinguish legacy-shape responses that were
    /// actually served by Jupiter under the hood.
    pub backend: &'static str,
}

pub async fn stake(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<StakeRequest>,
) -> Response {
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    do_swap_via_jupiter(state, "stake", WSOL_MINT.to_string(), INF_MINT.to_string(), req.amount, q.simulate).await
}

pub async fn unstake(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<UnstakeRequest>,
) -> Response {
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    do_swap_via_jupiter(state, "unstake", INF_MINT.to_string(), WSOL_MINT.to_string(), req.amount, q.simulate).await
}

async fn do_swap_via_jupiter(
    state: AppState,
    direction: &'static str,
    input_mint: String,
    output_mint: String,
    amount: u64,
    simulate: bool,
) -> Response {
    use axum::response::IntoResponse;

    let http = reqwest::Client::new();
    let amount_str = amount.to_string();
    let slip_str = LEGACY_SLIPPAGE_BPS.to_string();

    // 1. Quote
    let quote: SwapQuote = match http
        .get(format!("{JUPITER_LITE_API}/swap/v1/quote"))
        .query(&[
            ("inputMint", input_mint.as_str()),
            ("outputMint", output_mint.as_str()),
            ("amount", amount_str.as_str()),
            ("slippageBps", slip_str.as_str()),
        ])
        .send()
        .await
    {
        Ok(r) => match r.error_for_status() {
            Ok(r) => match r.json().await {
                Ok(q) => q,
                Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter quote decode: {e}")),
            },
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter quote: {e}")),
        },
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter quote network: {e}")),
    };
    let (quote_in, quote_out, _route_steps) = quote_summary(&quote);
    let amount_in = quote_in.unwrap_or(amount);
    let amount_out = quote_out.unwrap_or(0);

    // 2. Build swap tx
    let signer_pubkey = state.wallet.pubkey().to_string();
    let body = SwapBuildParams {
        quote_response: &quote,
        user_public_key: signer_pubkey,
        wrap_and_unwrap_sol: true,
    };
    let swap_resp: SwapBuildResp = match http
        .post(format!("{JUPITER_LITE_API}/swap/v1/swap"))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => match r.error_for_status() {
            Ok(r) => match r.json().await {
                Ok(s) => s,
                Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter swap decode: {e}")),
            },
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter swap: {e}")),
        },
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter swap network: {e}")),
    };

    let tx_bytes = match B64.decode(&swap_resp.swap_transaction) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("tx b64 decode: {e}")),
    };
    let tx: VersionedTransaction = match bincode::deserialize(&tx_bytes) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("tx deserialize: {e}")),
    };

    // 3. Sign + simulate or send (failover-aware via RpcContext methods)
    if simulate {
        match state.rpc.sign_existing_simulate(tx, state.wallet.keypair()).await {
            Ok(sim) => {
                let (layout_valid, summary) = classify_simulation(&sim);
                let logs = sim.logs.map(|l| l.into_iter().rev().take(20).rev().collect());
                Json(SanctumExecResponse {
                    txid: "<simulated>".to_string(),
                    direction,
                    amount_in,
                    amount_out,
                    fee_amount: 0,
                    fee_pct: "0".to_string(),
                    swap_src: SwapSrc::Jup,
                    simulated: true,
                    layout_valid: Some(layout_valid),
                    summary: Some(summary),
                    logs,
                    backend: "jupiter",
                })
                .into_response()
            }
            Err(e) => err(StatusCode::BAD_GATEWAY, format!("simulate: {e}")),
        }
    } else {
        match state.rpc.sign_existing_send(tx, state.wallet.keypair()).await {
            Ok(sig) => Json(SanctumExecResponse {
                txid: sig.to_string(),
                direction,
                amount_in,
                amount_out,
                fee_amount: 0,
                fee_pct: "0".to_string(),
                swap_src: SwapSrc::Jup,
                simulated: false,
                layout_valid: None,
                summary: None,
                logs: None,
                backend: "jupiter",
            })
            .into_response(),
            Err(e) => err(StatusCode::BAD_GATEWAY, format!("broadcast: {e}")),
        }
    }
}
