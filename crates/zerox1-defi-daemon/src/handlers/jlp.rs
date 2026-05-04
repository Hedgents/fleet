//! Jupiter Perpetuals — JLP mint / burn handlers.
//!
//! `POST /jlp/mint`   — deposit `asset` into the JLP pool, mint JLP
//! `POST /jlp/burn`   — burn JLP, receive `asset`
//!
//! Both accept `?simulate=true` to skip broadcast and only return klend's
//! program logs + layout-validity assessment.

use axum::{extract::{Query, State}, http::StatusCode, response::Response, Json};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use zerox1_defi_protocols::{
    constants::{
        JLP_MINT, USDC_MINT, USDT_MINT, WBTC_PORTAL_MINT, WETH_PORTAL_MINT, WSOL_MINT,
    },
    protocols::jlp::{add_liquidity_ix, remove_liquidity_ix, CustodyMeta, PoolMeta},
};

use crate::handlers::kamino::ExecQuery;
use crate::rpc::classify_simulation;
use crate::server::{err, AppState};

// JLP mint/burn fits comfortably in 400k CU once the named accounts are warm.
const JLP_CU_LIMIT: u32 = 400_000;
const JLP_PRIORITY_FEE: u64 = 10_000;

// ── Request / Response shapes ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MintRequest {
    /// Asset symbol — one of: usdc, usdt, sol, eth, btc.
    pub asset: String,
    /// Amount in raw units of the input asset (USDC/USDT = 6 dec, SOL = 9, ETH/BTC = 8).
    pub amount: u64,
    /// Optional minimum JLP out (raw, 6 decimals). Defaults to 0 (slippage off
    /// — fine for `?simulate=true`; real broadcasts should set this).
    #[serde(default)]
    pub min_lp_out: u64,
}

#[derive(Deserialize)]
pub struct BurnRequest {
    /// Asset symbol to receive — one of: usdc, usdt, sol, eth, btc.
    pub asset: String,
    /// Amount of JLP to burn (raw, 6 decimals).
    pub amount: u64,
    /// Optional minimum asset out (raw, in the asset's decimals). Defaults to 0.
    #[serde(default)]
    pub min_amount_out: u64,
}

#[derive(Serialize)]
pub struct JlpExecResponse {
    pub txid: String,
    pub direction: &'static str,
    pub asset: String,
    pub amount: u64,
    pub min_out: u64,
    pub simulated: bool,
    pub layout_valid: Option<bool>,
    pub summary: Option<String>,
    pub logs: Option<Vec<String>>,
}

// ── Asset symbol → mint resolution ──────────────────────────────────────────

fn mint_for_asset(asset: &str) -> Result<Pubkey, String> {
    match asset.to_ascii_lowercase().as_str() {
        "usdc" => Ok(USDC_MINT),
        "usdt" => Ok(USDT_MINT),
        "sol" | "wsol" => Ok(WSOL_MINT),
        "eth" | "weth" => Ok(WETH_PORTAL_MINT),
        "btc" | "wbtc" => Ok(WBTC_PORTAL_MINT),
        other => Err(format!(
            "asset {other} not in JLP pool (supported: usdc, usdt, sol, eth, btc)"
        )),
    }
}

fn resolve_custody<'a>(pool: &'a PoolMeta, asset: &str) -> Result<&'a CustodyMeta, String> {
    let mint = mint_for_asset(asset)?;
    pool.custody_for_mint(&mint)
        .ok_or_else(|| format!("custody for asset {asset} (mint {mint}) not present in loaded pool"))
}

// ── Handlers ────────────────────────────────────────────────────────────────

pub async fn mint(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<MintRequest>,
) -> Response {
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    let custody = match resolve_custody(&state.jlp_pool, &req.asset) {
        Ok(c) => c.clone(),
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let user = state.wallet.pubkey();

    let ixs = match add_liquidity_ix(&user, &state.jlp_pool, &custody, req.amount, req.min_lp_out) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    execute_or_simulate(
        &state,
        ixs,
        "mint",
        req.asset,
        req.amount,
        req.min_lp_out,
        q.simulate,
    )
    .await
}

pub async fn burn(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<BurnRequest>,
) -> Response {
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    let custody = match resolve_custody(&state.jlp_pool, &req.asset) {
        Ok(c) => c.clone(),
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let user = state.wallet.pubkey();

    let ixs = match remove_liquidity_ix(
        &user,
        &state.jlp_pool,
        &custody,
        req.amount,
        req.min_amount_out,
    ) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    execute_or_simulate(
        &state,
        ixs,
        "burn",
        req.asset,
        req.amount,
        req.min_amount_out,
        q.simulate,
    )
    .await
}

async fn execute_or_simulate(
    state: &AppState,
    ixs: Vec<solana_sdk::instruction::Instruction>,
    direction: &'static str,
    asset: String,
    amount: u64,
    min_out: u64,
    simulate: bool,
) -> Response {
    use axum::response::IntoResponse;

    if simulate {
        match state
            .rpc
            .build_sign_simulate(ixs, state.wallet.keypair(), JLP_CU_LIMIT, JLP_PRIORITY_FEE)
            .await
        {
            Ok(sim) => {
                let (layout_valid, summary) = classify_simulation(&sim);
                let logs = sim.logs.map(|l| l.into_iter().rev().take(20).rev().collect());
                Json(JlpExecResponse {
                    txid: "<simulated>".to_string(),
                    direction,
                    asset,
                    amount,
                    min_out,
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
            .build_sign_send(ixs, state.wallet.keypair(), JLP_CU_LIMIT, JLP_PRIORITY_FEE)
            .await
        {
            Ok(sig) => Json(JlpExecResponse {
                txid: sig.to_string(),
                direction,
                asset,
                amount,
                min_out,
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

// keep JLP_MINT reference live (used indirectly via PoolMeta but referenced
// for Cargo's dead-code warnings on the constants module).
const _: &Pubkey = &JLP_MINT;
