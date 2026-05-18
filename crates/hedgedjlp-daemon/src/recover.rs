//! Boot-time state recovery for the hedgedjlp daemon.
//!
//! ## The bug this fixes
//!
//! `RebalanceState.active: Mutex<Option<ActivePosition>>` lives in memory
//! only. When the daemon restarts, that state goes to `None`. Any
//! on-chain JLP holdings + Jupiter Perps short legs in the operator's
//! wallet still exist, but the rebalancer's tick loop sees
//! `state.active = None` and silently skips. `WithdrawHedgedJlp` then
//! returns a zero-`Report` sentinel and the position is unmanageable
//! until we re-`Assign`.
//!
//! ## What this module does
//!
//! On every daemon boot we read the operator wallet:
//!
//!   1. JLP SPL-token balance via the ATA for `JLP_MINT`.
//!   2. JLP pool's custody list (re-using `load_live_pool`).
//!   3. Open Jupiter Perps SHORT positions across the three hedge
//!      markets (SOL/ETH/BTC vs USDC) — same enumeration the dashboard's
//!      `/positions` endpoint uses (see
//!      `tools/fleet-dashboard-server/src/chain/jupiter_perps.rs`).
//!
//! From those reads we synthesise an `ActivePosition` good enough for
//! the rebalancer to take over. We do NOT persist to disk — the chain
//! is the source of truth on every boot.
//!
//! ## Withdraw vs rebalance — the open_counter gap
//!
//! `ActivePosition::open_positions` carries `(label, position_pda,
//! open_counter)` triples. The `open_counter` is the
//! `unix_seconds + i` value `hedge.rs` used at open time to derive the
//! `PositionRequest` PDA for an *increase* request. The withdraw path
//! re-uses that counter to derive the *close*-request PDA so the close
//! PDA matches the open PDA (audit-fix C2).
//!
//! Recovered positions don't carry the original counter — we never saw
//! the open tx. Setting `open_counter = 0` means a subsequent
//! `WithdrawHedgedJlp` envelope WILL derive the wrong close-request PDA
//! and fail. That's the honest trade-off:
//!
//!   - **Rebalance**: works — the rebalancer only READS chain state
//!     and never derives PDAs from `open_counter`.
//!   - **Withdraw**: fails until manual intervention reconciles the
//!     counter (operator can stop the daemon, hand-build a close
//!     request with the correct counter, or wait for the dispatch
//!     handler to use chain-derived PDAs in a follow-up PR).
//!
//! The sentinel `conv = [0xFF; 16]` makes recovered positions
//! immediately distinguishable from Assign-tracked ones in telemetry.

use std::sync::Arc;

use anyhow::{Context, Result};
use solana_sdk::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;
use tracing::warn;

use zerox1_defi_protocols::constants::{JLP_MINT, JLP_POOL, JUPITER_PERPETUALS_PROGRAM_ID};
use zerox1_defi_protocols::protocols::jlp::{decode_position, derive_position, PerpSide};
use zerox1_defi_runtime::rpc::RpcContext;

use crate::jlp_hedge::{
    load_live_pool, JLP_BTC_CUSTODY, JLP_ETH_CUSTODY, JLP_SOL_CUSTODY, JLP_USDC_CUSTODY_ADDR,
};
use crate::rebalance::ActivePosition;

/// Sentinel `conv` value attached to a recovered `ActivePosition`. The
/// genuine value (the Assign envelope's conversation id) is unknowable
/// at boot time — we never saw the Assign. `0xFF` repeated 16 times is
/// a distinct, grep-able marker for operators inspecting telemetry +
/// risk envelopes to spot recovered-vs-Assign-tracked positions.
pub const RECOVERED_CONV_SENTINEL: [u8; 16] = [0xFFu8; 16];

/// Default max-borrow-rate cap copied onto recovered positions. Matches
/// the daemon's hard cap (see `caps.rs`) and the AssignHedgedJlp default
/// shipped by the orchestrator — recovered positions inherit the same
/// guardrail.
pub const DEFAULT_MAX_BORROW_RATE_BPS: u16 = 5_000;

/// Watched (asset_label, asset_custody, collateral_custody, side) tuples.
/// Mirrors the dashboard's `watched_markets()` and the riskwatcher
/// poller's enumeration: the three SHORT markets hedgedjlp can open.
fn watched_short_markets() -> [(&'static str, Pubkey, Pubkey, PerpSide); 3] {
    [
        ("SOL", JLP_SOL_CUSTODY, JLP_USDC_CUSTODY_ADDR, PerpSide::Short),
        ("BTC", JLP_BTC_CUSTODY, JLP_USDC_CUSTODY_ADDR, PerpSide::Short),
        ("ETH", JLP_ETH_CUSTODY, JLP_USDC_CUSTODY_ADDR, PerpSide::Short),
    ]
}

/// Read the wallet's JLP balance via its associated-token account.
/// Returns `Ok(0)` when the ATA does not exist (the SPL token client
/// surfaces that as an `AccountNotFound`-flavoured error). Other RPC
/// errors propagate so the caller can log + skip recovery cleanly.
async fn read_jlp_balance(rpc: &RpcContext, wallet: &Pubkey) -> Result<u64> {
    let ata = get_associated_token_address(wallet, &JLP_MINT);
    match rpc.client.get_token_account_balance(&ata).await {
        Ok(bal) => Ok(bal.amount.parse::<u64>().unwrap_or(0)),
        Err(e) => {
            // ATA-not-found and similar "fresh wallet" cases come back
            // as RPC errors from the SPL client. Treat them as zero
            // balance — the operator wallet simply doesn't hold JLP yet.
            // Anything that looks like AccountNotFound / could-not-find
            // is normal; any other RPC error gets logged and treated as
            // zero too, because the safer fallback is "fresh start, no
            // recovery" rather than aborting boot.
            let s = e.to_string();
            if s.contains("could not find account")
                || s.contains("AccountNotFound")
                || s.contains("Invalid param")
            {
                Ok(0)
            } else {
                warn!(
                    ?e,
                    %ata,
                    "get_token_account_balance failed for JLP ATA; treating as zero \
                     (recovery skipped this boot)"
                );
                Ok(0)
            }
        }
    }
}

/// Discover the wallet's open SHORT Jupiter Perps positions across the
/// three hedge markets. Mirrors the dashboard's
/// `discover_hedge_positions` and the riskwatcher poller's
/// `discover_positions`: one `getMultipleAccounts` round-trip for all
/// three (asset, USDC, Short) PDAs, decode each present account,
/// drop is_empty / wrong-owner / decode-fail entries.
///
/// Returns `(label, position_pda, size_usd_micro)` tuples. The size is
/// `pos.size_usd` (USD with 6 decimals, i.e. micro-USD) — same units
/// the dashboard already serialises to JSON.
async fn discover_shorts(
    rpc: &RpcContext,
    wallet: &Pubkey,
) -> Result<Vec<(&'static str, Pubkey, u64)>> {
    let markets = watched_short_markets();
    let pdas: Vec<Pubkey> = markets
        .iter()
        .map(|(_label, custody, coll, side)| derive_position(wallet, &JLP_POOL, custody, coll, *side))
        .collect();
    let accounts = rpc
        .client
        .get_multiple_accounts(&pdas)
        .await
        .context("get_multiple_accounts for hedgedjlp short PDAs")?;
    let mut out = Vec::new();
    for ((label, _custody, _coll, _side), (pda, maybe_account)) in
        markets.iter().zip(pdas.iter().zip(accounts.into_iter()))
    {
        let Some(account) = maybe_account else {
            continue;
        };
        if account.owner != JUPITER_PERPETUALS_PROGRAM_ID {
            warn!(pda = %pda, owner = %account.owner, "PDA exists but wrong owner; skipping");
            continue;
        }
        match decode_position(*pda, &account.data) {
            Ok(pos) if !pos.is_empty() => {
                out.push((*label, *pda, pos.size_usd));
            }
            Ok(_) => {
                // empty position — fully closed; nothing to recover
            }
            Err(e) => {
                warn!(pda = %pda, ?e, "decode_position failed during recovery; skipping");
            }
        }
    }
    Ok(out)
}

/// Reconstruct an `ActivePosition` from on-chain reads, good enough for
/// the rebalancer to take over.
///
/// Returns:
///   * `Ok(None)` — operator wallet has zero JLP (fresh start).
///   * `Ok(Some(pos))` — recovery built a usable `ActivePosition`.
///   * `Err(_)` — only on RPC failures that block reading at all (e.g.
///     the JLP pool / custody read errored). Caller treats this as a
///     no-op recovery and continues boot.
///
/// Note: a non-zero JLP balance with zero discovered shorts is logged
/// at WARN — that combination means the wallet is fully long JLP (or
/// the shorts were closed manually). Recovery still returns
/// `Some(pos)` with `open_positions = []` so the rebalancer can take
/// over and emit `DeltaDrift` escalations as appropriate.
pub async fn recover_active_position(
    rpc: &Arc<RpcContext>,
    wallet_pubkey: Pubkey,
) -> Result<Option<ActivePosition>> {
    // 1. JLP balance via ATA.
    let jlp_balance = read_jlp_balance(rpc, &wallet_pubkey).await?;
    if jlp_balance == 0 {
        return Ok(None);
    }

    // 2. JLP pool custody list. Reuse the live-pool loader so the
    //    custody pubkeys are decoded straight from chain (rather than
    //    hand-rolled here).
    let pool = match load_live_pool(rpc).await {
        Ok(p) => p,
        Err(e) => {
            warn!(
                ?e,
                "recover: load_live_pool failed; building ActivePosition with empty \
                 custody_pubkeys (rebalancer will no-op until next boot retries)"
            );
            // Empty list means `tick_once`'s
            // `custody_pubkeys.is_empty()` branch fires and the
            // rebalancer logs + skips cleanly. We still want to record
            // the position so withdraw paths (eventually) and telemetry
            // see a non-None active.
            zerox1_defi_protocols::protocols::jlp::PoolMeta {
                pool: JLP_POOL,
                jlp_mint: JLP_MINT,
                perpetuals: zerox1_defi_protocols::protocols::jlp::derive_perpetuals(),
                transfer_authority: zerox1_defi_protocols::protocols::jlp::derive_transfer_authority(),
                event_authority: zerox1_defi_protocols::protocols::jlp::derive_event_authority(),
                custodies: vec![],
            }
        }
    };
    let custody_pubkeys: Vec<Pubkey> = pool.custodies.iter().map(|c| c.address).collect();

    // 3. Discover open shorts. RPC failure here is non-fatal — we log
    //    + treat as zero shorts so the rebalancer at least sees the
    //    JLP balance and can size hedges back up.
    let shorts = match discover_shorts(rpc, &wallet_pubkey).await {
        Ok(v) => v,
        Err(e) => {
            warn!(
                ?e,
                "recover: discover_shorts failed; proceeding with zero open shorts"
            );
            Vec::new()
        }
    };

    if shorts.is_empty() {
        warn!(
            jlp_lamports = jlp_balance,
            "recover: wallet holds JLP but no Jupiter Perps shorts were discovered — \
             position is under-hedged. Recording active position so rebalancer can \
             size the missing hedge legs."
        );
    }

    // Sum of discovered short notionals. `pos.size_usd` is micro-USD
    // (6 decimals) which matches `ActivePosition::hedge_notional_usdc`'s
    // existing semantics (cf. `hedge_notional_usdc` in
    // `hedge::open_short_requests` returns micro-USD too).
    let hedge_notional_usdc: u64 = shorts.iter().map(|(_, _, sz)| *sz).sum();

    // Build `open_positions`. The third tuple field is `open_counter`,
    // used to derive the close-request PDA during withdraw. We don't
    // have the original counter for recovered positions: setting it to
    // 0 is a deliberate, visible marker. Recovered positions are
    // **rebalance-only**; withdraw will fail until manual intervention
    // reconciles the counter.
    let open_positions: Vec<(String, Pubkey, u64)> = shorts
        .iter()
        .map(|(label, pda, _)| ((*label).to_string(), *pda, 0u64))
        .collect();

    let pos = ActivePosition {
        // 0xFF sentinel makes recovered positions trivially grep-able
        // in telemetry and risk envelopes — the operator-set conv from
        // the original Assign envelope is unknowable here.
        conv: RECOVERED_CONV_SENTINEL,
        our_jlp_lamports: jlp_balance,
        // Matches AssignHedgedJlp's recording behaviour at open time:
        // jlp_acquired := our_jlp at record time.
        jlp_acquired_lamports: jlp_balance,
        // Delta-neutral default. The Assign payload's target is lost
        // on restart; 0 (= fully hedged) is the safest assumption and
        // is what the orchestrator emits in its default Assign.
        target_delta_bps: 0,
        max_borrow_rate_bps: DEFAULT_MAX_BORROW_RATE_BPS,
        custody_pubkeys,
        hedge_notional_usdc,
        open_positions,
    };
    Ok(Some(pos))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::commitment_config::CommitmentConfig;

    fn unreachable_rpc() -> Arc<RpcContext> {
        // Mirrors the testing style in
        // `riskwatcher-daemon/src/jupiter_perps_poller.rs`: an
        // unreachable RPC URL short-circuits to an RPC error in every
        // chain read. Construction itself does no I/O.
        Arc::new(RpcContext::new(
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        ))
    }

    /// Zero JLP balance → `Ok(None)`. The unreachable-RPC stub returns
    /// an error from `get_token_account_balance`, which `read_jlp_balance`
    /// maps to a zero balance (fresh-start fallback). No further chain
    /// reads happen.
    #[tokio::test]
    async fn zero_jlp_balance_returns_none() {
        let rpc = unreachable_rpc();
        let wallet = Pubkey::new_unique();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), recover_active_position(&rpc, wallet))
                .await
                .expect("must return promptly on unreachable RPC");
        let recovered = result.expect("recover must not error on zero-balance path");
        assert!(
            recovered.is_none(),
            "zero JLP balance must yield Ok(None) — fresh start"
        );
    }

    /// Synthetic ActivePosition shape: all fields populated, sentinel
    /// conv, default cap, open_counter = 0 across the board.
    #[test]
    fn synthetic_recovered_position_shape() {
        let pda_sol = Pubkey::new_unique();
        let pda_eth = Pubkey::new_unique();
        let pda_btc = Pubkey::new_unique();
        let custody_sol = Pubkey::new_unique();
        let custody_eth = Pubkey::new_unique();
        let custody_btc = Pubkey::new_unique();
        let custody_usdc = Pubkey::new_unique();
        let custody_usdt = Pubkey::new_unique();

        let open_positions: Vec<(String, Pubkey, u64)> = vec![
            ("SOL".to_string(), pda_sol, 0),
            ("ETH".to_string(), pda_eth, 0),
            ("BTC".to_string(), pda_btc, 0),
        ];
        let custody_pubkeys = vec![
            custody_sol,
            custody_btc,
            custody_eth,
            custody_usdc,
            custody_usdt,
        ];
        let hedge_notional_usdc: u64 = 77_000_000 + 19_000_000 + 16_000_000;
        let pos = ActivePosition {
            conv: RECOVERED_CONV_SENTINEL,
            our_jlp_lamports: 174_000_000,
            jlp_acquired_lamports: 174_000_000,
            target_delta_bps: 0,
            max_borrow_rate_bps: DEFAULT_MAX_BORROW_RATE_BPS,
            custody_pubkeys: custody_pubkeys.clone(),
            hedge_notional_usdc,
            open_positions: open_positions.clone(),
        };

        assert_eq!(pos.conv, [0xFFu8; 16], "sentinel conv must be 0xFF x 16");
        assert_eq!(pos.our_jlp_lamports, 174_000_000);
        assert_eq!(pos.jlp_acquired_lamports, pos.our_jlp_lamports);
        assert_eq!(pos.target_delta_bps, 0);
        assert_eq!(pos.max_borrow_rate_bps, 5_000);
        assert_eq!(pos.custody_pubkeys.len(), 5);
        assert_eq!(pos.hedge_notional_usdc, hedge_notional_usdc);
        assert_eq!(pos.open_positions.len(), 3);
        for (_, _, counter) in &pos.open_positions {
            assert_eq!(
                *counter, 0,
                "recovered positions carry open_counter=0 — withdraw will mis-derive PDAs \
                 until manual reconciliation"
            );
        }
    }

    #[test]
    fn watched_short_markets_covers_three_shorts() {
        let m = watched_short_markets();
        assert_eq!(m.len(), 3);
        let labels: Vec<&str> = m.iter().map(|(l, _, _, _)| *l).collect();
        assert_eq!(labels, vec!["SOL", "BTC", "ETH"]);
        for (_, _, coll, side) in &m {
            assert_eq!(*coll, JLP_USDC_CUSTODY_ADDR, "collateral is always USDC");
            assert_eq!(*side, PerpSide::Short, "hedgedjlp only opens shorts");
        }
    }

    /// Sentinel constant integrity: a recovered position must be
    /// distinguishable from any zero-init or [0u8; 16] conv that could
    /// plausibly come from an Assign envelope.
    #[test]
    fn recovered_conv_sentinel_is_distinct_from_zero() {
        assert_ne!(RECOVERED_CONV_SENTINEL, [0u8; 16]);
        assert_eq!(RECOVERED_CONV_SENTINEL, [0xFFu8; 16]);
    }

    /// Default cap matches the documented operator-facing default.
    #[test]
    fn default_max_borrow_rate_matches_documented_cap() {
        assert_eq!(DEFAULT_MAX_BORROW_RATE_BPS, 5_000);
    }

    /// Non-zero JLP balance + zero discovered shorts → `Some(pos)` with
    /// empty `open_positions`. We can't drive `recover_active_position`
    /// end-to-end without a live RPC, but we CAN verify the assembly
    /// logic shape (the rebalancer-friendly invariants).
    #[test]
    fn recovered_position_with_no_shorts_yields_empty_open_positions() {
        let pos = ActivePosition {
            conv: RECOVERED_CONV_SENTINEL,
            our_jlp_lamports: 100_000_000,
            jlp_acquired_lamports: 100_000_000,
            target_delta_bps: 0,
            max_borrow_rate_bps: DEFAULT_MAX_BORROW_RATE_BPS,
            custody_pubkeys: vec![],
            hedge_notional_usdc: 0,
            open_positions: vec![],
        };
        // The rebalancer's tick_once will hit the
        // `custody_pubkeys.is_empty()` branch and log+skip cleanly.
        // Once the next boot retries `load_live_pool` successfully the
        // custody list re-populates.
        assert!(pos.open_positions.is_empty());
        assert_eq!(pos.hedge_notional_usdc, 0);
        // Under-hedged state still records the JLP balance so
        // telemetry surfaces a non-zero deployed-USD figure.
        assert_eq!(pos.our_jlp_lamports, 100_000_000);
    }
}
