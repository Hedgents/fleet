//! Kamino HTTP handlers.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};

use zerox1_defi_protocols::protocols::kamino::{
    borrow_obligation_liquidity_ix, deposit_ix, repay_obligation_liquidity_ix, withdraw_ix,
};

use crate::rpc::classify_simulation;
use crate::server::{err, AppState};

// ── Compute budget defaults ─────────────────────────────────────────────────
//
// InitializeObligation + ATA-create + RefreshReserve + deposit fits under
// 500_000 CU. Multiply (when shipped) will need ~1_000_000.
const KAMINO_CU_LIMIT: u32 = 500_000;
const KAMINO_PRIORITY_FEE: u64 = 10_000; // 0.00001 SOL per CU at the limit

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
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    let reserve = match state.kamino_reserve(&req.asset) {
        Some(r) => r,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                format!(
                    "asset {} not supported (use usdc, sol, or jitosol)",
                    req.asset
                ),
            )
        }
    };
    let user = state.wallet.pubkey();

    // NOTE: pre-existing handler (defi-daemon). Passing &[] here preserves the
    // pre-v0.1.5 RefreshObligation shape; deposits to an obligation that
    // already has registered reserves will fail with InvalidAccountInput. The
    // stable-yield-daemon path (which is what 0x01fi mainnet uses) does pass
    // the real reserve list via fetch_obligation_reserves.
    let ixs = match deposit_ix(&user, &reserve, req.amount, (0, 0), &[]) {
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
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    let reserve = match state.kamino_reserve(&req.asset) {
        Some(r) => r,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                format!(
                    "asset {} not supported (use usdc, sol, or jitosol)",
                    req.asset
                ),
            )
        }
    };
    let user = state.wallet.pubkey();

    let ixs = match withdraw_ix(&user, &reserve, req.amount, (0, 0), &[]) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    execute_or_simulate(&state, ixs, req.asset, req.amount, q.simulate).await
}

// ── Borrow / Repay ──────────────────────────────────────────────────────────
//
// The Multiply Agent uses these to lever up: deposit jitoSOL → borrow SOL →
// (swap externally) → deposit again. Repay reverses the leverage.

#[derive(Deserialize)]
pub struct BorrowRequest {
    /// Asset to borrow — "usdc", "sol", or "jitosol".
    pub asset: String,
    /// Amount in raw units of the borrow asset.
    pub amount: u64,
}

#[derive(Deserialize)]
pub struct RepayRequest {
    pub asset: String,
    pub amount: u64,
}

pub async fn borrow(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<BorrowRequest>,
) -> Response {
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    let reserve = match state.kamino_reserve(&req.asset) {
        Some(r) => r,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                format!(
                    "asset {} not supported (use usdc, sol, or jitosol)",
                    req.asset
                ),
            )
        }
    };
    let user = state.wallet.pubkey();

    let ixs = match borrow_obligation_liquidity_ix(&user, &reserve, req.amount, (0, 0)) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    execute_or_simulate(&state, ixs, req.asset, req.amount, q.simulate).await
}

pub async fn repay(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<RepayRequest>,
) -> Response {
    if req.amount == 0 {
        return err(StatusCode::BAD_REQUEST, "amount must be > 0");
    }
    let reserve = match state.kamino_reserve(&req.asset) {
        Some(r) => r,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                format!(
                    "asset {} not supported (use usdc, sol, or jitosol)",
                    req.asset
                ),
            )
        }
    };
    let user = state.wallet.pubkey();

    let ixs = match repay_obligation_liquidity_ix(&user, &reserve, req.amount, (0, 0)) {
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
            .build_sign_simulate(
                ixs,
                state.wallet.keypair(),
                KAMINO_CU_LIMIT,
                KAMINO_PRIORITY_FEE,
            )
            .await
        {
            Ok(sim) => {
                let (layout_valid, summary) = classify_simulation(&sim);
                let logs = sim
                    .logs
                    .map(|l| l.into_iter().rev().take(20).rev().collect());
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
            .build_sign_send(
                ixs,
                state.wallet.keypair(),
                KAMINO_CU_LIMIT,
                KAMINO_PRIORITY_FEE,
            )
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
