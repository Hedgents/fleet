//! Jupiter swap aggregator handlers.
//!
//! `POST /swap`              — generic input/output swap (any two SPL mints)
//! `POST /swap/sol-to-inf`   — convenience: stake SOL → INF (replaces broken
//!                              Sanctum router)
//! `POST /swap/inf-to-sol`   — convenience: unstake INF → SOL
//!
//! All routes accept `?simulate=true` to get the layout-validity report
//! without broadcasting.
//!
//! Architecture: same pattern as the original Sanctum integration —
//! quote → POST swap → b64 decode → bincode deserialize → sign → simulate
//! or send. The only difference is that Jupiter returns a tx wrapped in an
//! address-lookup-table-aware versioned tx; our existing `sign_existing_send`
//! handles that.

use axum::{extract::{Query, State}, http::StatusCode, response::Response, Json};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};

use zerox1_defi_protocols::{
    constants::{INF_MINT, WSOL_MINT},
    protocols::jupiter::{
        quote_summary, SwapBuildParams, SwapBuildResp, SwapQuote, JUPITER_LITE_API,
    },
};

use crate::handlers::kamino::ExecQuery;
use crate::rpc::classify_simulation;
use crate::server::{err, AppState};

/// Default slippage tolerance. 50 bps = 0.5%, conservative for stable-pair
/// LST swaps; callers can override via the request body.
const DEFAULT_SLIPPAGE_BPS: u16 = 50;

// ── Request / Response shapes ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SwapRequest {
    /// Input mint (base58 pubkey).
    pub input_mint: String,
    /// Output mint (base58 pubkey).
    pub output_mint: String,
    /// Amount in raw units of the input mint.
    pub amount: u64,
    /// Slippage in basis points; defaults to 50 (0.5%).
    #[serde(default)]
    pub slippage_bps: Option<u16>,
}

#[derive(Deserialize)]
pub struct StakeRequest {
    /// Amount in raw lamports (1 SOL = 1_000_000_000).
    pub amount: u64,
    #[serde(default)]
    pub slippage_bps: Option<u16>,
}

#[derive(Deserialize)]
pub struct UnstakeRequest {
    /// Amount in raw INF units (INF has 9 decimals).
    pub amount: u64,
    #[serde(default)]
    pub slippage_bps: Option<u16>,
}

#[derive(Serialize)]
pub struct SwapExecResponse {
    pub txid: String,
    pub direction: String,
    pub input_mint: String,
    pub output_mint: String,
    pub amount_in: u64,
    pub amount_out: u64,
    pub route_steps: usize,
    pub slippage_bps: u16,
    pub simulated: bool,
    pub layout_valid: Option<bool>,
    pub summary: Option<String>,
    pub logs: Option<Vec<String>>,
}

// ── Handlers ────────────────────────────────────────────────────────────────

pub async fn swap(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<SwapRequest>,
) -> Response {
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    if Pubkey::try_from(req.input_mint.as_str()).is_err() {
        return err(StatusCode::BAD_REQUEST, "invalid input_mint");
    }
    if Pubkey::try_from(req.output_mint.as_str()).is_err() {
        return err(StatusCode::BAD_REQUEST, "invalid output_mint");
    }
    let slippage_bps = req.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS);
    let direction = format!("{}->{}", short(&req.input_mint), short(&req.output_mint));
    do_swap(state, direction, req.input_mint, req.output_mint, req.amount, slippage_bps, q.simulate).await
}

pub async fn stake_sol_to_inf(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<StakeRequest>,
) -> Response {
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    let slippage_bps = req.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS);
    do_swap(
        state,
        "stake-sol-to-inf".to_string(),
        WSOL_MINT.to_string(),
        INF_MINT.to_string(),
        req.amount,
        slippage_bps,
        q.simulate,
    )
    .await
}

pub async fn unstake_inf_to_sol(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<UnstakeRequest>,
) -> Response {
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    let slippage_bps = req.slippage_bps.unwrap_or(DEFAULT_SLIPPAGE_BPS);
    do_swap(
        state,
        "unstake-inf-to-sol".to_string(),
        INF_MINT.to_string(),
        WSOL_MINT.to_string(),
        req.amount,
        slippage_bps,
        q.simulate,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn do_swap(
    state: AppState,
    direction: String,
    input_mint: String,
    output_mint: String,
    amount: u64,
    slippage_bps: u16,
    simulate: bool,
) -> Response {
    use axum::response::IntoResponse;

    let http = reqwest::Client::new();
    let amount_str = amount.to_string();
    let slip_str = slippage_bps.to_string();

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
                Err(e) => return err(StatusCode::BAD_GATEWAY, format!("quote decode: {e}")),
            },
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter quote: {e}")),
        },
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter quote network: {e}")),
    };
    let (quote_in, quote_out, route_steps) = quote_summary(&quote);
    let amount_in = quote_in.unwrap_or(amount);
    let amount_out = quote_out.unwrap_or(0);

    // 2. Build the swap tx
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
                Err(e) => return err(StatusCode::BAD_GATEWAY, format!("swap decode: {e}")),
            },
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter swap: {e}")),
        },
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("jupiter swap network: {e}")),
    };

    // 3. Decode the b64 versioned tx
    let tx_bytes = match B64.decode(&swap_resp.swap_transaction) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("tx b64 decode: {e}")),
    };
    let tx: VersionedTransaction = match bincode::deserialize(&tx_bytes) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("tx deserialize: {e}")),
    };

    // 4. Sign + simulate or send
    if simulate {
        match state.rpc.sign_existing_simulate(tx, state.wallet.keypair()).await {
            Ok(sim) => {
                let (layout_valid, summary) = classify_simulation(&sim);
                let logs = sim.logs.map(|l| l.into_iter().rev().take(20).rev().collect());
                Json(SwapExecResponse {
                    txid: "<simulated>".to_string(),
                    direction,
                    input_mint,
                    output_mint,
                    amount_in,
                    amount_out,
                    route_steps,
                    slippage_bps,
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
        match state.rpc.sign_existing_send(tx, state.wallet.keypair()).await {
            Ok(sig) => Json(SwapExecResponse {
                txid: sig.to_string(),
                direction,
                input_mint,
                output_mint,
                amount_in,
                amount_out,
                route_steps,
                slippage_bps,
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

fn short(mint: &str) -> String {
    if mint.len() <= 8 {
        mint.to_string()
    } else {
        format!("{}..", &mint[..6])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_mint_truncates_long_keys() {
        assert_eq!(short("So11111111111111111111111111111111111111112"), "So1111..");
        assert_eq!(short("abc"), "abc");
    }
}
