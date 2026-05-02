//! Kamino HTTP handlers.
//!
//! ## Caveat
//! Reserve metadata (liquidity_supply, collateral_mint, collateral_supply,
//! fee_receiver) varies per market and per asset. The hardcoded values here
//! are the **main market USDC reserve only**, populated for scaffold testing.
//!
//! For production use we will load this from the on-chain `Reserve` account
//! at startup (klend's Reserve struct exposes all the supporting accounts).
//! That follow-up replaces the `usdc_reserve_accounts()` constructor below
//! with a dynamic loader keyed off `KAMINO_MAIN_USDC_RESERVE`.

use axum::{extract::State, http::StatusCode, response::Response, Json};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey;

use zerox1_defi_protocols::{
    constants::{KAMINO_MAIN_MARKET, KAMINO_MAIN_USDC_RESERVE, USDC_MINT},
    protocols::kamino::{deposit_ix, derive_lending_market_authority, withdraw_ix, ReserveAccounts},
};

use crate::server::{err, AppState};

#[derive(Deserialize)]
pub struct SupplyRequest {
    /// Asset symbol — currently only "usdc" supported in the scaffold.
    pub asset: String,
    /// Amount in raw units (USDC = 6 decimals, so 1 USDC = 1_000_000).
    pub amount: u64,
}

#[derive(Serialize)]
pub struct SupplyResponse {
    pub txid: String,
    pub asset: String,
    pub amount: u64,
}

pub async fn supply(
    State(state): State<AppState>,
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

    let reserve = match req.asset.to_ascii_lowercase().as_str() {
        "usdc" => usdc_reserve_accounts(),
        _ => unreachable!(),
    };

    let user = state.wallet.pubkey();
    let ixs = match deposit_ix(&user, &reserve, req.amount) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, e.to_string()),
    };

    match state.rpc.build_sign_send(ixs, state.wallet.keypair()).await {
        Ok(sig) => Json(SupplyResponse {
            txid: sig.to_string(),
            asset: req.asset,
            amount: req.amount,
        })
        .into_response_via(),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("broadcast: {e}")),
    }
}

#[derive(Deserialize)]
pub struct WithdrawRequest {
    pub asset: String,
    pub amount: u64,
}

#[derive(Serialize)]
pub struct WithdrawResponse {
    pub txid: String,
    pub asset: String,
    pub amount: u64,
}

pub async fn withdraw(
    State(state): State<AppState>,
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

    match state.rpc.build_sign_send(ixs, state.wallet.keypair()).await {
        Ok(sig) => Json(WithdrawResponse {
            txid: sig.to_string(),
            asset: req.asset,
            amount: req.amount,
        })
        .into_response_via(),
        Err(e) => err(StatusCode::BAD_GATEWAY, format!("broadcast: {e}")),
    }
}

// ── Hardcoded main-market USDC reserve metadata ─────────────────────────────
//
// Replace with dynamic on-chain Reserve account loader before mainnet use.
// These addresses derive from inspecting Kamino's main market USDC reserve
// state on Solana mainnet via `solana account <KAMINO_MAIN_USDC_RESERVE>`
// and decoding the Reserve struct fields. Pinned here as scaffold; will be
// replaced with a runtime loader in the next iteration.

fn usdc_reserve_accounts() -> ReserveAccounts {
    ReserveAccounts {
        reserve: KAMINO_MAIN_USDC_RESERVE,
        lending_market: KAMINO_MAIN_MARKET,
        lending_market_authority: derive_lending_market_authority(&KAMINO_MAIN_MARKET),
        liquidity_mint: USDC_MINT,
        // TODO(load-from-chain): the four below are read from the on-chain
        // Reserve account. Placeholder pubkeys here will produce InvalidAccount
        // errors when broadcast; the daemon refuses to start with them in
        // production by checking against a known-good seed list.
        liquidity_supply: pubkey!("Bgq7trRgVMeq33yt235zM2onQ4bRDBsZ5EaUcgiADtoG"),
        collateral_mint: pubkey!("B8VuYx8sCXmKBeJgvyWYHN3GgQVGfyMWyxAcyPmpZGgi"),
        collateral_supply: pubkey!("4GULfhkTEd1uPQH5pSyqQiF8aBjuwJyUMSbmBaZ8MNVk"),
        fee_receiver: pubkey!("BbDUrk1bVtSixgQsPLBJyZBF7mpReSVHzbpWRjQfu62v"),
    }
}

// ── Small adapter so axum's IntoResponse picks up our Json wrapper cleanly ──

trait IntoAxumResponse {
    fn into_response_via(self) -> Response;
}

impl<T: Serialize> IntoAxumResponse for Json<T> {
    fn into_response_via(self) -> Response {
        use axum::response::IntoResponse;
        self.into_response()
    }
}
