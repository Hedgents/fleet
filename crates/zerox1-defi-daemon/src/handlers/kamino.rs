//! Kamino HTTP handlers.
//!
//! ## Caveat
//!
//! Reserve metadata (liquidity_supply, collateral_mint, collateral_supply,
//! fee_receiver) varies per market and per asset. The hardcoded values here
//! are **placeholders for the main market USDC reserve**. They will not pass
//! klend's account validation on broadcast.
//!
//! Two safe paths until the on-chain Reserve loader ships:
//!   1. Use `?simulate=true` (or `--simulate` from the CLI) — runs the
//!      transaction through `simulateTransaction` against the configured
//!      RPC. Returns klend's program logs without spending SOL.
//!   2. Replace the placeholders below with real on-chain account values
//!      pulled via `solana account <KAMINO_MAIN_USDC_RESERVE>` decoded
//!      against klend's Reserve struct definition.

use axum::{extract::{Query, State}, http::StatusCode, response::Response, Json};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey;

use zerox1_defi_protocols::{
    constants::{KAMINO_MAIN_MARKET, KAMINO_MAIN_USDC_RESERVE, USDC_MINT},
    protocols::kamino::{deposit_ix, derive_lending_market_authority, withdraw_ix, ReserveAccounts},
};

use crate::rpc::classify_simulation;
use crate::server::{err, AppState};

// ── Compute budget defaults ─────────────────────────────────────────────────
//
// klend deposit/withdraw + ATA-create + refresh fits comfortably under
// 400_000 CU on mainnet. Multiply (when shipped) will need ~1_000_000.
const KAMINO_CU_LIMIT: u32 = 400_000;
const KAMINO_PRIORITY_FEE: u64 = 10_000;  // 0.00001 SOL per CU at the limit

// ── Query flags shared across all DeFi endpoints ────────────────────────────

#[derive(Deserialize, Default)]
pub struct ExecQuery {
    /// `?simulate=true` to run the transaction through `simulateTransaction`
    /// instead of broadcasting. Returns layout validity + program logs.
    #[serde(default)]
    pub simulate: bool,
}

// ── Request / Response shapes ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SupplyRequest {
    /// Asset symbol — currently only "usdc" supported in the scaffold.
    pub asset: String,
    /// Amount in raw units (USDC = 6 decimals, so 1 USDC = 1_000_000).
    pub amount: u64,
}

#[derive(Serialize)]
pub struct ExecResponse {
    /// Solana transaction signature when broadcast; "<simulated>" when sim.
    pub txid: String,
    pub asset: String,
    pub amount: u64,
    /// True if simulated rather than broadcast.
    pub simulated: bool,
    /// True if simulation passed klend's account validation. None when broadcast.
    pub layout_valid: Option<bool>,
    /// Simulation summary or error string. None on successful broadcast.
    pub summary: Option<String>,
    /// Program logs from simulation (truncated to last 20 lines). None on broadcast.
    pub logs: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct WithdrawRequest {
    pub asset: String,
    pub amount: u64,
}

// ── Handlers ────────────────────────────────────────────────────────────────

pub async fn supply(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<SupplyRequest>,
) -> Response {
    if req.asset.to_ascii_lowercase() != "usdc" {
        return err(
            StatusCode::BAD_REQUEST,
            format!("asset {} not supported (scaffold supports usdc only)", req.asset),
        );
    }
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }

    let reserve = usdc_reserve_accounts();
    let user = state.wallet.pubkey();

    let ixs = match deposit_ix(&user, &reserve, req.amount) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    execute_or_simulate(&state, ixs, req.asset, req.amount, q.simulate).await
}

pub async fn withdraw(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<WithdrawRequest>,
) -> Response {
    if req.asset.to_ascii_lowercase() != "usdc" {
        return err(
            StatusCode::BAD_REQUEST,
            format!("asset {} not supported (scaffold supports usdc only)", req.asset),
        );
    }
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }

    let reserve = usdc_reserve_accounts();
    let user = state.wallet.pubkey();

    let ixs = match withdraw_ix(&user, &reserve, req.amount) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    execute_or_simulate(&state, ixs, req.asset, req.amount, q.simulate).await
}

async fn execute_or_simulate(
    state: &AppState,
    ixs: Vec<solana_sdk::instruction::Instruction>,
    asset: String,
    amount: u64,
    simulate: bool,
) -> Response {
    use axum::response::IntoResponse;

    if simulate {
        match state
            .rpc
            .build_sign_simulate(ixs, state.wallet.keypair(), KAMINO_CU_LIMIT, KAMINO_PRIORITY_FEE)
            .await
        {
            Ok(sim) => {
                let (layout_valid, summary) = classify_simulation(&sim);
                let logs = sim.logs.map(|l| l.into_iter().rev().take(20).rev().collect());
                Json(ExecResponse {
                    txid: "<simulated>".to_string(),
                    asset,
                    amount,
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
            .build_sign_send(ixs, state.wallet.keypair(), KAMINO_CU_LIMIT, KAMINO_PRIORITY_FEE)
            .await
        {
            Ok(sig) => Json(ExecResponse {
                txid: sig.to_string(),
                asset,
                amount,
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

// ── Hardcoded main-market USDC reserve metadata ─────────────────────────────
//
// PLACEHOLDER VALUES. Replace before mainnet broadcast. Use
// `?simulate=true` to verify layout against the live klend program — the
// simulation runs free, returns klend's program logs, and tells you whether
// the account ordering is correct.

fn usdc_reserve_accounts() -> ReserveAccounts {
    ReserveAccounts {
        reserve: KAMINO_MAIN_USDC_RESERVE,
        lending_market: KAMINO_MAIN_MARKET,
        lending_market_authority: derive_lending_market_authority(&KAMINO_MAIN_MARKET),
        liquidity_mint: USDC_MINT,
        liquidity_supply: pubkey!("Bgq7trRgVMeq33yt235zM2onQ4bRDBsZ5EaUcgiADtoG"),
        collateral_mint:  pubkey!("B8VuYx8sCXmKBeJgvyWYHN3GgQVGfyMWyxAcyPmpZGgi"),
        collateral_supply: pubkey!("4GULfhkTEd1uPQH5pSyqQiF8aBjuwJyUMSbmBaZ8MNVk"),
        fee_receiver: pubkey!("BbDUrk1bVtSixgQsPLBJyZBF7mpReSVHzbpWRjQfu62v"),
    }
}
