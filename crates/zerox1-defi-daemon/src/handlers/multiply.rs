//! Kamino Multiply — iterative leverage step.
//!
//! `POST /multiply/lever-up` composes one leverage iteration in a single
//! transaction:
//!
//! ```text
//! 1. kamino borrow SOL   → user wSOL ATA       (3 ixs: ATA-create + refresh + borrow)
//! 2. spl-token CloseAccount on wSOL ATA → SOL goes to user wallet
//! 3. jito DepositSol     → user jitoSOL ATA    (2 ixs: ATA-create + DepositSol)
//! 4. kamino refresh + deposit jitoSOL collateral (2 ixs)
//! ```
//!
//! Total: 8 instructions, ~22 unique accounts, fits comfortably under the
//! 1232-byte v0 transaction limit. No flash loan, no address-lookup-table
//! plumbing.
//!
//! The Multiply Agent calls this 2-3 times to reach a target leverage
//! (e.g. 2.5×). Each call increases position size by `borrow_sol_amount`.
//! Slippage exposure between iterations is negligible (each tx confirms in
//! ~30s and the SOL/jitoSOL exchange rate is governed by the Jito stake
//! pool, not a market).
//!
//! ## Pre-requisites
//!
//! - User must already have an obligation initialized (via a prior
//!   `POST /kamino/supply` of jitoSOL).
//! - User must already have jitoSOL collateral deposited that the borrow
//!   borrows against.
//! - Caller must compute `expected_jitosol_received` from
//!   `borrow_sol_amount × current_jitosol_per_sol_rate × (1 - safety_margin)`.
//!   Pass slightly under the expected amount to ensure the deposit succeeds
//!   even with mid-tx exchange-rate drift.

use axum::{extract::{Query, State}, http::StatusCode, response::Response, Json};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::{Deserialize, Serialize};
use solana_sdk::{instruction::Instruction, transaction::VersionedTransaction};
use spl_token::instruction::close_account;

use zerox1_defi_protocols::{
    constants::{JITOSOL_MINT, TOKEN_PROGRAM_ID, WSOL_MINT},
    protocols::{
        jito::deposit_sol_ix,
        jupiter::{
            quote_summary, SwapBuildParams, SwapBuildResp, SwapQuote, JUPITER_LITE_API,
        },
        kamino::{
            borrow_obligation_liquidity_ix, deposit_collateral_only_ix,
            refresh_reserve_ix, repay_obligation_liquidity_ix, withdraw_ix,
        },
    },
    util::ata,
};

use crate::handlers::kamino::ExecQuery;
use crate::rpc::classify_simulation;
use crate::server::{err, AppState};

// ~700k CU is enough for 8 ixs (kamino deposit alone needs ~200k, jito
// DepositSol ~150k, kamino borrow ~150k, plus overhead).
const MULTIPLY_CU_LIMIT: u32 = 800_000;
const MULTIPLY_PRIORITY_FEE: u64 = 10_000;

#[derive(Deserialize)]
pub struct LeverUpRequest {
    /// Amount of SOL to borrow this iteration, in raw lamports.
    /// (1 SOL = 1_000_000_000)
    pub borrow_sol_amount: u64,
    /// Expected jitoSOL to receive from staking the borrowed SOL — caller
    /// computes from the current Jito stake-pool exchange rate, with a
    /// small safety margin (e.g. 0.5%) so mid-tx drift doesn't fail the
    /// deposit. In raw jitoSOL units (9 decimals).
    pub expected_jitosol_received: u64,
}

#[derive(Serialize)]
pub struct LeverUpResponse {
    pub txid: String,
    pub borrow_sol_amount: u64,
    pub deposited_jitosol_amount: u64,
    pub instruction_count: usize,
    pub simulated: bool,
    pub layout_valid: Option<bool>,
    pub summary: Option<String>,
    pub logs: Option<Vec<String>>,
}

pub async fn lever_up(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<LeverUpRequest>,
) -> Response {
    use axum::response::IntoResponse;

    if req.borrow_sol_amount == 0 {
        return err(StatusCode::BAD_REQUEST, "borrow_sol_amount must be > 0");
    }
    if req.expected_jitosol_received == 0 {
        return err(StatusCode::BAD_REQUEST, "expected_jitosol_received must be > 0");
    }

    let user = state.wallet.pubkey();
    let user_wsol_ata = ata(&user, &WSOL_MINT);

    let sol_reserve = state.kamino_sol_reserve.as_ref().clone();
    let jitosol_reserve = state.kamino_jitosol_reserve.as_ref().clone();

    // ── Step 1: borrow SOL (3 ixs: ATA-create wSOL + refresh sol_reserve + borrow)
    let mut ixs: Vec<Instruction> = match borrow_obligation_liquidity_ix(
        &user,
        &sol_reserve,
        req.borrow_sol_amount,
    ) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    // ── Step 2: close the wSOL ATA so the lamports flow to the user wallet
    // (Jito DepositSol takes raw SOL, not wSOL).
    let close_wsol = match close_account(
        &TOKEN_PROGRAM_ID,
        &user_wsol_ata,
        &user,         // destination — user's main wallet receives the SOL
        &user,         // authority
        &[],           // no multisig
    ) {
        Ok(ix) => ix,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("close_account: {e}")),
    };
    ixs.push(close_wsol);

    // ── Step 3: Jito DepositSol → user's jitoSOL ATA  (2 ixs: ATA-create + DepositSol)
    let jito_ixs = match deposit_sol_ix(&user, &state.jito_pool, req.borrow_sol_amount) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    ixs.extend(jito_ixs);

    // ── Step 4: refresh jitoSOL reserve + deposit jitoSOL as collateral
    ixs.push(refresh_reserve_ix(&jitosol_reserve));
    let deposit_collateral = match deposit_collateral_only_ix(
        &user,
        &jitosol_reserve,
        req.expected_jitosol_received,
    ) {
        Ok(ix) => ix,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };
    ixs.push(deposit_collateral);

    let ix_count = ixs.len();

    if q.simulate {
        match state
            .rpc
            .build_sign_simulate(ixs, state.wallet.keypair(), MULTIPLY_CU_LIMIT, MULTIPLY_PRIORITY_FEE)
            .await
        {
            Ok(sim) => {
                let (layout_valid, summary) = classify_simulation(&sim);
                let logs = sim.logs.map(|l| l.into_iter().rev().take(20).rev().collect());
                Json(LeverUpResponse {
                    txid: "<simulated>".to_string(),
                    borrow_sol_amount: req.borrow_sol_amount,
                    deposited_jitosol_amount: req.expected_jitosol_received,
                    instruction_count: ix_count,
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
            .build_sign_send(ixs, state.wallet.keypair(), MULTIPLY_CU_LIMIT, MULTIPLY_PRIORITY_FEE)
            .await
        {
            Ok(sig) => Json(LeverUpResponse {
                txid: sig.to_string(),
                borrow_sol_amount: req.borrow_sol_amount,
                deposited_jitosol_amount: req.expected_jitosol_received,
                instruction_count: ix_count,
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

// ── /multiply/lever-down ────────────────────────────────────────────────────
//
// Reverses leverage by decomposing into three sequential transactions:
//
// ```text
// tx 1: kamino withdraw jitoSOL collateral   (3 ixs)
// tx 2: jupiter swap jitoSOL → wSOL          (Jupiter's tx with its own ALT)
// tx 3: kamino repay SOL                     (3 ixs)
// ```
//
// Why three txs instead of one atomic close: Jupiter's swap tx carries its
// own address-lookup-table addresses; composing it into our own v0 tx
// alongside withdraw + repay would require ALT extraction (separate task).
// Each tx's confirmation is required before the next can succeed (tx 2 needs
// the jitoSOL withdrawn in tx 1; tx 3 needs the wSOL produced by tx 2).
//
// In `?simulate=true` mode each phase is simulated independently and the
// results returned in one response. The simulations don't actually depend
// on each other being broadcast — `simulateTransaction` works against the
// pre-existing chain state.

#[derive(Deserialize)]
pub struct LeverDownRequest {
    /// Amount of jitoSOL collateral to withdraw, raw units (9 decimals).
    pub withdraw_jitosol_amount: u64,
    /// Amount of SOL debt to repay, raw lamports.
    pub repay_sol_amount: u64,
    /// Slippage for the jitoSOL→SOL swap, in basis points. Defaults to 50 (0.5%).
    #[serde(default)]
    pub slippage_bps: Option<u16>,
}

#[derive(Serialize)]
pub struct PhaseResult {
    pub txid: String,
    pub instruction_count: usize,
    pub layout_valid: Option<bool>,
    pub summary: Option<String>,
    /// Quote details for the swap phase only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_out: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_steps: Option<usize>,
}

#[derive(Serialize)]
pub struct LeverDownResponse {
    pub simulated: bool,
    pub withdraw_jitosol_amount: u64,
    pub repay_sol_amount: u64,
    pub slippage_bps: u16,
    pub withdraw: PhaseResult,
    pub swap: PhaseResult,
    pub repay: PhaseResult,
}

const LEVER_DOWN_DEFAULT_SLIPPAGE_BPS: u16 = 50;

pub async fn lever_down(
    State(state): State<AppState>,
    Query(q): Query<ExecQuery>,
    Json(req): Json<LeverDownRequest>,
) -> Response {
    use axum::response::IntoResponse;

    if req.withdraw_jitosol_amount == 0 {
        return err(StatusCode::BAD_REQUEST, "withdraw_jitosol_amount must be > 0");
    }
    if req.repay_sol_amount == 0 {
        return err(StatusCode::BAD_REQUEST, "repay_sol_amount must be > 0");
    }
    let slippage_bps = req.slippage_bps.unwrap_or(LEVER_DOWN_DEFAULT_SLIPPAGE_BPS);

    let user = state.wallet.pubkey();
    let jitosol_reserve = state.kamino_jitosol_reserve.as_ref().clone();
    let sol_reserve = state.kamino_sol_reserve.as_ref().clone();

    // ── Phase 1: Build the withdraw tx ──────────────────────────────────────
    let withdraw_ixs: Vec<Instruction> =
        match withdraw_ix(&user, &jitosol_reserve, req.withdraw_jitosol_amount) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("withdraw build: {e}")),
        };
    let withdraw_ix_count = withdraw_ixs.len();

    // ── Phase 2: Fetch Jupiter quote + swap tx ──────────────────────────────
    let http = reqwest::Client::new();
    let amount_str = req.withdraw_jitosol_amount.to_string();
    let slip_str = slippage_bps.to_string();
    let jitosol_mint = JITOSOL_MINT.to_string();
    let wsol_mint = WSOL_MINT.to_string();

    let quote: SwapQuote = match http
        .get(format!("{JUPITER_LITE_API}/swap/v1/quote"))
        .query(&[
            ("inputMint", jitosol_mint.as_str()),
            ("outputMint", wsol_mint.as_str()),
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
    let (quote_in, quote_out, route_steps) = quote_summary(&quote);

    // wrap_and_unwrap_sol: false — we WANT the swap output to land in the
    // user's wSOL ATA so the repay step can take from it directly. If we
    // unwrapped to raw SOL we'd need an extra wrap step before repay.
    let signer_pubkey = user.to_string();
    let body = SwapBuildParams {
        quote_response: &quote,
        user_public_key: signer_pubkey,
        wrap_and_unwrap_sol: false,
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
    let swap_tx_bytes = match B64.decode(&swap_resp.swap_transaction) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("swap tx b64 decode: {e}")),
    };
    let swap_tx: VersionedTransaction = match bincode::deserialize(&swap_tx_bytes) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("swap tx deserialize: {e}")),
    };

    // ── Phase 3: Build the repay tx ─────────────────────────────────────────
    let repay_ixs: Vec<Instruction> =
        match repay_obligation_liquidity_ix(&user, &sol_reserve, req.repay_sol_amount) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("repay build: {e}")),
        };
    let repay_ix_count = repay_ixs.len();

    // ── Execute or simulate ─────────────────────────────────────────────────
    if q.simulate {
        let withdraw_fut = state.rpc.build_sign_simulate(
            withdraw_ixs,
            state.wallet.keypair(),
            MULTIPLY_CU_LIMIT,
            MULTIPLY_PRIORITY_FEE,
        );
        let swap_fut =
            state.rpc.sign_existing_simulate(swap_tx, state.wallet.keypair());
        let repay_fut = state.rpc.build_sign_simulate(
            repay_ixs,
            state.wallet.keypair(),
            MULTIPLY_CU_LIMIT,
            MULTIPLY_PRIORITY_FEE,
        );
        let (w_res, s_res, r_res) = tokio::join!(withdraw_fut, swap_fut, repay_fut);

        let withdraw = match w_res {
            Ok(sim) => phase_from_simulation(sim, withdraw_ix_count, None, None, None),
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("withdraw simulate: {e}")),
        };
        let swap = match s_res {
            Ok(sim) => phase_from_simulation(sim, 1, quote_in, quote_out, Some(route_steps)),
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("swap simulate: {e}")),
        };
        let repay = match r_res {
            Ok(sim) => phase_from_simulation(sim, repay_ix_count, None, None, None),
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("repay simulate: {e}")),
        };

        Json(LeverDownResponse {
            simulated: true,
            withdraw_jitosol_amount: req.withdraw_jitosol_amount,
            repay_sol_amount: req.repay_sol_amount,
            slippage_bps,
            withdraw,
            swap,
            repay,
        })
        .into_response()
    } else {
        // Strict ordering: each tx must confirm before the next is sent (tx 2
        // depends on the jitoSOL withdrawn in tx 1; tx 3 depends on the wSOL
        // produced in tx 2). `build_sign_send` already calls
        // send_and_confirm_transaction so each await blocks on confirmation.
        let withdraw_sig = match state
            .rpc
            .build_sign_send(withdraw_ixs, state.wallet.keypair(), MULTIPLY_CU_LIMIT, MULTIPLY_PRIORITY_FEE)
            .await
        {
            Ok(sig) => sig,
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("withdraw broadcast: {e}")),
        };
        let swap_sig = match state.rpc.sign_existing_send(swap_tx, state.wallet.keypair()).await {
            Ok(sig) => sig,
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("swap broadcast: {e}")),
        };
        let repay_sig = match state
            .rpc
            .build_sign_send(repay_ixs, state.wallet.keypair(), MULTIPLY_CU_LIMIT, MULTIPLY_PRIORITY_FEE)
            .await
        {
            Ok(sig) => sig,
            Err(e) => return err(StatusCode::BAD_GATEWAY, format!("repay broadcast: {e}")),
        };

        Json(LeverDownResponse {
            simulated: false,
            withdraw_jitosol_amount: req.withdraw_jitosol_amount,
            repay_sol_amount: req.repay_sol_amount,
            slippage_bps,
            withdraw: PhaseResult {
                txid: withdraw_sig.to_string(),
                instruction_count: withdraw_ix_count,
                layout_valid: None,
                summary: None,
                quote_in: None,
                quote_out: None,
                route_steps: None,
            },
            swap: PhaseResult {
                txid: swap_sig.to_string(),
                instruction_count: 1,
                layout_valid: None,
                summary: None,
                quote_in,
                quote_out,
                route_steps: Some(route_steps),
            },
            repay: PhaseResult {
                txid: repay_sig.to_string(),
                instruction_count: repay_ix_count,
                layout_valid: None,
                summary: None,
                quote_in: None,
                quote_out: None,
                route_steps: None,
            },
        })
        .into_response()
    }
}

fn phase_from_simulation(
    sim: solana_rpc_client_api::response::RpcSimulateTransactionResult,
    ix_count: usize,
    quote_in: Option<u64>,
    quote_out: Option<u64>,
    route_steps: Option<usize>,
) -> PhaseResult {
    let (layout_valid, summary) = classify_simulation(&sim);
    PhaseResult {
        txid: "<simulated>".to_string(),
        instruction_count: ix_count,
        layout_valid: Some(layout_valid),
        summary: Some(summary),
        quote_in,
        quote_out,
        route_steps,
    }
}
