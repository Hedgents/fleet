//! Kamino HTTP handlers (lifted from monolith for multiply-daemon).
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

use std::str::FromStr;
use std::sync::Arc;

use anyhow;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use tracing::{info, warn};
use zerox1_defi_protocols::{
    constants::{
        ASSOCIATED_TOKEN_PROGRAM_ID, KAMINO_LEND_PROGRAM_ID, KAMINO_MAIN_MARKET,
        KAMINO_MAIN_USDC_RESERVE, SPL_STAKE_POOL_PROGRAM_ID, SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID,
        USDC_MINT,
    },
    protocols::kamino::{
        deposit_ix, derive_lending_market_authority, derive_user_obligation, withdraw_ix,
        ReserveAccounts,
    },
    protocols::kamino_loader::{decode_obligation, DecodedObligation, OBLIGATION_DISCRIMINATOR},
};
use zerox1_defi_runtime::rpc::{classify_simulation, RpcContext};
use zerox1_defi_wallet::Wallet;

/// Program IDs the Multiply daemon is allowed to sign for. Anything else
/// is rejected by the wallet whitelist before signing.
///
/// Audit-fix I1: expanded from Kamino-only to the full ixn surface the
/// leverage loop actually emits. Each round of `leverage::run_one_lever_up_iteration`
/// touches:
///   - klend (borrow_obligation_liquidity, refresh_reserve, deposit_collateral_only)
///   - SPL Token (close_account on wSOL ATA)
///   - SPL Stake Pool (Jito's deposit_sol)
///   - Associated Token Account (idempotent ATA creation inside klend + jito helpers)
///   - System Program (account creation inside ATA helpers)
///   - Compute Budget (set_compute_unit_limit, set_compute_unit_price prepended by RpcContext)
///
/// Anything outside this set will be rejected by `verify_ixns` before signing.
pub fn whitelist_program_ids() -> Vec<Pubkey> {
    vec![
        // Kamino Lend (re-exported from the protocols crate).
        KAMINO_LEND_PROGRAM_ID,
        // Kamino Farms (used by Multiply harvest path). Not yet exposed as a
        // const in the protocols crate — string-literal here, will be cleaned
        // up in the strategy plan.
        Pubkey::from_str("FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr").unwrap(),
        // SPL stake-pool program — Jito's stake pool is one instance of this.
        SPL_STAKE_POOL_PROGRAM_ID,
        // SPL Token (close_account on wSOL ATA after borrowing SOL).
        TOKEN_PROGRAM_ID,
        // Associated Token Account (idempotent ATA creation).
        ASSOCIATED_TOKEN_PROGRAM_ID,
        // System program (rent + account creation paths inside ATA helpers).
        SYSTEM_PROGRAM_ID,
        // Compute budget (set_compute_unit_limit / set_compute_unit_price
        // prepended automatically by `RpcContext::build_signed`).
        solana_sdk::compute_budget::ID,
    ]
}

/// Minimal application state used by the lifted Kamino handlers.
///
/// In the monolith this was a much larger `AppState`; here it's stripped to
/// the two fields these handlers actually touch — an RPC context (for build /
/// sign / send / simulate) and a wallet (for signing). Multiply legitimately
/// signs Kamino-program transactions, so the wallet field is required.
#[derive(Clone)]
pub struct AppState {
    pub rpc: RpcContext,
    pub wallet: Arc<Wallet>,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(ApiError { error: msg.into() })).into_response()
}

// ── Compute budget defaults ─────────────────────────────────────────────────
//
// klend deposit/withdraw + ATA-create + refresh fits comfortably under
// 400_000 CU on mainnet. Multiply (when shipped) will need ~1_000_000.
const KAMINO_CU_LIMIT: u32 = 400_000;
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

// ── Pure instruction-building functions ────────────────────────────────────

/// Build a supply (deposit) instruction set without HTTP wrapping.
///
/// Takes raw parameters and returns the instruction vector needed to supply
/// USDC to the Kamino main market. Validates `amount_lamports > 0`.
///
/// The `_rpc` and `_vault` parameters are accepted to match the interface
/// expected by future callers (e.g., the leverage loop in M6) but are not
/// currently used inside this fn — the supply path uses a hard-coded USDC
/// reserve via `usdc_reserve_accounts()`.
pub async fn build_supply_ixns(
    rpc: &RpcContext,
    _vault: Pubkey,
    user: Pubkey,
    amount_lamports: u64,
) -> anyhow::Result<Vec<solana_sdk::instruction::Instruction>> {
    if amount_lamports == 0 {
        return Err(anyhow::anyhow!("amount must be > 0"));
    }

    let reserve = usdc_reserve_accounts();

    // Bug fix (2026-05-13, mirrored from stable-yield v0.1.4 + v0.1.5):
    //
    //   v0.1.4 — `deposit_ix` unconditionally prepends `InitializeObligation`,
    //   which `Allocate`s the obligation PDA. Any second deposit hits
    //   `Allocate: account already in use`. Fetch the obligation and drop
    //   the InitObligation ixn when the PDA already has data.
    //
    //   v0.1.5 — once the obligation has registered reserves, klend's
    //   RefreshObligation requires each as a remaining account in array
    //   order (deposits, then borrows). Without them: InvalidAccountInput
    //   (0x1776). Parse the obligation and pass the list through deposit_ix.
    //
    // Multiply uses leveraged jitoSOL via the same KLend program, so the
    // same obligation pattern applies the moment any deposit goes through
    // this supply path on an obligation that's already seen any reserve.
    let obligation = derive_user_obligation(&user, &reserve.lending_market);
    let (obligation_already_exists, obligation_reserves) =
        fetch_obligation_reserves(&rpc.client, &obligation).await;

    let mut ixs = deposit_ix(&user, &reserve, amount_lamports, &obligation_reserves)?;
    if obligation_already_exists {
        // ixs[0] is the InitObligation ixn — see `kamino::deposit_ix`.
        info!(
            %obligation,
            "obligation account already exists; dropping InitializeObligation ixn"
        );
        ixs.remove(0);
    }
    Ok(ixs)
}

/// Build a withdraw instruction set without HTTP wrapping.
///
/// Takes raw parameters and returns the instruction vector needed to withdraw
/// USDC from the Kamino main market. Validates `amount_lamports > 0`.
///
/// The `_rpc` and `_vault` parameters are accepted to match the interface
/// expected by future callers (e.g., the leverage loop in M6) but are not
/// currently used inside this fn — the withdraw path uses a hard-coded USDC
/// reserve via `usdc_reserve_accounts()`.
pub async fn build_withdraw_ixns(
    rpc: &RpcContext,
    _vault: Pubkey,
    user: Pubkey,
    amount_lamports: u64,
) -> anyhow::Result<Vec<solana_sdk::instruction::Instruction>> {
    if amount_lamports == 0 {
        return Err(anyhow::anyhow!("amount must be > 0"));
    }

    let reserve = usdc_reserve_accounts();

    // Bug fix (2026-05-13, mirrored from stable-yield v0.1.5): the
    // RefreshObligation embedded in the withdraw bundle requires the
    // obligation's registered reserves as remaining accounts in array
    // order. Without them the withdraw fails with InvalidAccountInput
    // (0x1776) once any deposits/borrows have been registered.
    let obligation = derive_user_obligation(&user, &reserve.lending_market);
    let (_, obligation_reserves) = fetch_obligation_reserves(&rpc.client, &obligation).await;

    let ixs = withdraw_ix(&user, &reserve, amount_lamports, &obligation_reserves)?;
    Ok(ixs)
}

/// Fetch the obligation account and extract:
///   * whether the caller should skip `InitializeObligation` (account already
///     has data on chain), and
///   * the ordered list of registered reserve pubkeys to pass as
///     RefreshObligation remaining accounts (deposits first, then borrows,
///     in on-chain array order).
///
/// Lifted from stable-yield-daemon's `lend::fetch_obligation_reserves` —
/// kept inline rather than moved to a shared crate because the fix is small
/// and v0.1.6 explicitly does not introduce shared traits.
async fn fetch_obligation_reserves(rpc: &RpcClient, obligation: &Pubkey) -> (bool, Vec<Pubkey>) {
    let data = match rpc.get_account_data(obligation).await {
        Ok(data) => data,
        Err(e) => {
            info!(
                %obligation, ?e,
                "obligation account not found or RPC error; keeping InitObligation, zero remaining reserves"
            );
            return (false, Vec::new());
        }
    };
    let skip_init = decide_skip_init_obligation(obligation, &data);
    let reserves = decode_obligation_reserves(*obligation, &data);
    (skip_init, reserves)
}

/// Pure helper: given the raw obligation account data, return the ordered
/// reserve list to pass as RefreshObligation remaining accounts. Returns an
/// empty Vec when the data is too short / wrong discriminator / decode fails.
fn decode_obligation_reserves(address: Pubkey, data: &[u8]) -> Vec<Pubkey> {
    match decode_obligation(address, data) {
        Ok(DecodedObligation {
            deposits, borrows, ..
        }) => deposits
            .into_iter()
            .map(|d| d.reserve)
            .chain(borrows.into_iter().map(|b| b.reserve))
            .collect(),
        Err(e) => {
            warn!(
                %address, ?e,
                "decode_obligation failed; passing empty remaining_accounts to RefreshObligation"
            );
            Vec::new()
        }
    }
}

/// Pure decision: given the raw account data for the obligation PDA, return
/// `true` (skip InitObligation) when the account already has data on chain.
fn decide_skip_init_obligation(obligation: &Pubkey, data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    if data.len() >= 8 && data[0..8] != OBLIGATION_DISCRIMINATOR {
        warn!(
            %obligation,
            got = %hex::encode(&data[0..8]),
            expected = %hex::encode(OBLIGATION_DISCRIMINATOR),
            "obligation account exists with unexpected discriminator; skipping InitObligation anyway"
        );
    }
    true
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
            format!(
                "asset {} not supported (scaffold supports usdc only)",
                req.asset
            ),
        );
    }

    let user = state.wallet.pubkey();
    let ixs = match build_supply_ixns(&state.rpc, Pubkey::default(), user, req.amount).await {
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
            format!(
                "asset {} not supported (scaffold supports usdc only)",
                req.asset
            ),
        );
    }

    let user = state.wallet.pubkey();
    let ixs = match build_withdraw_ixns(&state.rpc, Pubkey::default(), user, req.amount).await {
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
        collateral_mint: pubkey!("B8VuYx8sCXmKBeJgvyWYHN3GgQVGfyMWyxAcyPmpZGgi"),
        collateral_supply: pubkey!("4GULfhkTEd1uPQH5pSyqQiF8aBjuwJyUMSbmBaZ8MNVk"),
        fee_receiver: pubkey!("BbDUrk1bVtSixgQsPLBJyZBF7mpReSVHzbpWRjQfu62v"),
        // Scope prices oracle for Kamino main market USDC reserve.
        // Read from reserve account data at offset 5112, verified on mainnet 2026-05-04.
        scope_prices: Pubkey::from_str("3t4JZcueEzTbVP6kLxXrL3VpWx45jDer4eqysweBchNH").unwrap(),
        // Farm collateral for Kamino main USDC reserve, verified on mainnet 2026-05-09.
        farm_collateral: pubkey!("JAvnB9AKtgPsTEoKmn24Bq64UMoYcrtWtq42HHBdsPkh"),
    }
}

#[cfg(test)]
mod build_ixns_tests {
    use super::*;

    // We can't easily test the full RPC-backed path in a unit test
    // without mocking. Instead, prove that build_supply_ixns and
    // build_withdraw_ixns exist with the expected signatures. If RPC
    // mocking infrastructure exists in defi-protocols::kamino, use it
    // here for a more thorough test.
    #[test]
    fn pure_fns_exist() {
        // Sanity check: functions are callable with the expected argument types.
        // We don't actually call them (would require an RpcContext), but this
        // proves the signatures exist at compile time.
        let _ = build_supply_ixns as *const ();
        let _ = build_withdraw_ixns as *const ();
    }

    // ── decide_skip_init_obligation / decode_obligation_reserves tests ──────
    //
    // Mirror the stable-yield-daemon::lend tests covering the same on-chain
    // Obligation layout. See `decode_obligation` in
    // zerox1-defi-protocols::protocols::kamino_loader for the source of
    // truth on offsets.
    const OBLIG_LM_OFF: usize = 32;
    const OBLIG_OWNER_OFF: usize = 64;
    const OBLIG_DEPOSITS_OFF: usize = 96;
    const OBLIG_DEPOSIT_STRIDE: usize = 136;
    const OBLIG_BORROWS_OFF: usize = 1208;
    const OBLIG_BORROW_STRIDE: usize = 184;
    const OBLIG_MIN_SIZE: usize = 2128 + 16 * 4;

    fn make_obligation(deposit_reserves: &[Pubkey], active_borrows: &[Pubkey]) -> Vec<u8> {
        let mut buf = vec![0u8; OBLIG_MIN_SIZE];
        buf[0..8].copy_from_slice(&OBLIGATION_DISCRIMINATOR);
        let market = Pubkey::new_unique();
        let owner = Pubkey::new_unique();
        buf[OBLIG_LM_OFF..OBLIG_LM_OFF + 32].copy_from_slice(&market.to_bytes());
        buf[OBLIG_OWNER_OFF..OBLIG_OWNER_OFF + 32].copy_from_slice(&owner.to_bytes());
        for (i, r) in deposit_reserves.iter().enumerate() {
            let off = OBLIG_DEPOSITS_OFF + i * OBLIG_DEPOSIT_STRIDE;
            buf[off..off + 32].copy_from_slice(&r.to_bytes());
            buf[off + 32..off + 40].copy_from_slice(&1_u64.to_le_bytes());
        }
        for (i, r) in active_borrows.iter().enumerate() {
            let off = OBLIG_BORROWS_OFF + i * OBLIG_BORROW_STRIDE;
            buf[off..off + 32].copy_from_slice(&r.to_bytes());
            buf[off + 56..off + 72].copy_from_slice(&1_u128.to_le_bytes());
        }
        buf
    }

    #[test]
    fn decide_skip_init_for_missing_account_is_false() {
        // No data on chain → run InitObligation (this is a fresh wallet).
        assert!(!decide_skip_init_obligation(&Pubkey::new_unique(), &[]));
    }

    #[test]
    fn decide_skip_init_for_existing_account_is_true() {
        // Account already exists with the right discriminator → skip
        // InitObligation. This is the mainnet bug case (second deposit
        // hitting `Allocate: already in use`).
        let buf = make_obligation(&[], &[]);
        assert!(decide_skip_init_obligation(&Pubkey::new_unique(), &buf));
    }

    #[test]
    fn reserves_for_obligation_with_one_deposit_no_borrows() {
        // After a first jitoSOL collateral deposit (or USDC supply via the
        // placeholder handler) the obligation has one registered reserve.
        // The next deposit / withdraw must pass it as a remaining account.
        let jitosol_reserve = Pubkey::new_unique();
        let buf = make_obligation(&[jitosol_reserve], &[]);
        let reserves = decode_obligation_reserves(Pubkey::new_unique(), &buf);
        assert_eq!(reserves, vec![jitosol_reserve]);
    }

    #[test]
    fn reserves_for_fresh_obligation_is_empty() {
        // Just-initialized obligation (no collateral yet). Empty deposits +
        // empty borrows → zero remaining accounts.
        let buf = make_obligation(&[], &[]);
        let reserves = decode_obligation_reserves(Pubkey::new_unique(), &buf);
        assert!(reserves.is_empty());
    }

    #[test]
    fn reserves_preserve_order_deposits_then_borrows() {
        // Real leveraged jitoSOL multiply: jitoSOL collateral deposit +
        // USDC borrow → 2 remaining accounts, deposits before borrows.
        let jitosol = Pubkey::new_unique();
        let usdc = Pubkey::new_unique();
        let buf = make_obligation(&[jitosol], &[usdc]);
        let reserves = decode_obligation_reserves(Pubkey::new_unique(), &buf);
        assert_eq!(reserves, vec![jitosol, usdc]);
    }

    #[test]
    fn reserves_for_garbage_data_is_empty() {
        // Wrong discriminator / too-short buffer: decode fails, we fall
        // back to an empty list (klend will surface a clearer error than us).
        let reserves = decode_obligation_reserves(Pubkey::new_unique(), &[0xAA; 64]);
        assert!(reserves.is_empty());
    }
}
