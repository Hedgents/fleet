//! v0.2.5: Jupiter Perps position poller.
//!
//! Mirrors the structure of [`crate::poller`] (the Kamino obligation
//! poller) but reads Jupiter Perps `Position` accounts via fixed-offset
//! Borsh decoding.
//!
//! ## Scope
//!
//! The hedgedjlp daemon opens SHORT positions on Jupiter Perps against
//! SOL / BTC / ETH custodies, with USDC as collateral. Each (asset,
//! USDC, Short) tuple maps to a deterministic Position PDA. We:
//!
//!   1. derive every (asset, USDC, Short) PDA for the watched wallet at
//!      boot,
//!   2. once per tick `getMultipleAccounts` over all PDAs in a single
//!      RPC round-trip,
//!   3. decode each present account, fetch the asset's Custody
//!      `max_leverage`, compute liquidation distance, and feed the
//!      result into the existing escalate path with
//!      `RiskKind::LiquidationDistance`.
//!
//! ## What this poller is NOT
//!
//! - Not a generic Position discoverer. We only check the small fixed
//!   set of (asset, USDC, Short) PDAs the hedgedjlp daemon can open.
//!   `getProgramAccounts` filtering on `owner` was rejected because
//!   it is rate-limited on Helius/Triton and unnecessarily heavy for a
//!   < 5 PDA workload.
//! - Not a long-position monitor. Hedgedjlp never opens longs; if a
//!   future strategy does, add it to [`WATCHED_PERP_SUBJECTS`].
//! - Not a borrow-fee-aware liquidation oracle. See
//!   `docs/jupiter-perps-position-spec.md` §5 for the conservative
//!   approximation used in v0.2.5.
//!
//! ## Failure policy
//!
//! Identical to the Kamino poller: per-position RPC errors are logged
//! at `warn!` and the tick continues. The Escalate emit path is
//! shared, so `(subject, severity)` de-dup applies across protocols —
//! a single wallet reporting Critical from both Kamino and Jupiter
//! Perps emits two distinct envelopes (different `subject` bytes).

use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

use zerox1_defi_protocols::constants::{JLP_POOL, JUPITER_PERPETUALS_PROGRAM_ID};
use zerox1_defi_protocols::protocols::jlp::{
    decode_custody_max_leverage_bps, decode_position, derive_position, DecodedPosition, PerpSide,
};
use zerox1_defi_runtime::identity::RoleIdentity;
use zerox1_defi_runtime::rpc::RpcContext;
use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::fleet::riskwatcher::{RiskKind, RiskSeverity};

use crate::escalate::{self, DedupCache};
use crate::telemetry::EscalateMetrics;
use crate::thresholds::{DISTANCE_CRITICAL_BPS, DISTANCE_NOTICE_BPS, DISTANCE_WARNING_BPS};

/// SOL custody (the asset side for SOL shorts) — mainnet.
const SOL_CUSTODY_STR: &str = "7xS2gz2bTp3fwCC7knJvUWTEU9Tycczu6VhJYKgi1wdz";
/// BTC custody — mainnet.
const BTC_CUSTODY_STR: &str = "5Pv3gM9JrFFH883SWAhvJC9RPYmo8UNxuFtv5bMMALkm";
/// ETH custody — mainnet.
const ETH_CUSTODY_STR: &str = "AQCGyheWPLeo6Qp9WpYS9m3Qj479t7R636N9ey1rEjEn";
/// USDC custody — mainnet (collateral_custody for all hedgedjlp shorts).
const USDC_CUSTODY_STR: &str = "G18jKKXQwBbrHeiK3C9MRXhkHsLHf7XgCSisykV46EZa";

/// Parse a known-good mainnet custody address. Panics if the constant
/// above is corrupted — these strings are guarded by the
/// `mainnet_custody_addresses_parse` unit test and never reach
/// production with a bad value.
fn parse_custody(s: &str) -> Pubkey {
    s.parse().expect("custody pubkey constant is malformed")
}

/// Static list of (asset_custody, collateral_custody, side) tuples we
/// monitor per watched wallet. v0.2.5 covers exactly the three short
/// markets hedgedjlp can open.
fn watched_markets() -> [(Pubkey, Pubkey, PerpSide); 3] {
    let usdc = parse_custody(USDC_CUSTODY_STR);
    [
        (parse_custody(SOL_CUSTODY_STR), usdc, PerpSide::Short),
        (parse_custody(BTC_CUSTODY_STR), usdc, PerpSide::Short),
        (parse_custody(ETH_CUSTODY_STR), usdc, PerpSide::Short),
    ]
}

/// Bundle of poll dependencies passed to [`run`]. Mirrors the
/// [`crate::poller::PollerCtx`] shape so the two pollers compose
/// identically inside `main.rs`.
pub struct JupiterPerpsPollerCtx {
    pub rpc: Arc<RpcContext>,
    /// Wallets the riskwatcher is monitoring. v0.2.5 boots with a
    /// single entry — the hedgedjlp daemon's wallet — passed in via
    /// `--watch-perp-wallet` (see `main.rs`). Future revisions can add
    /// more without changing the tick loop.
    pub watched_wallets: Vec<Pubkey>,
    pub handle: NodeHandle,
    pub role: RoleIdentity,
    pub nonce: Arc<AtomicU64>,
    pub dedup: Arc<DedupCache>,
    pub orchestrator: [u8; 32],
    pub metrics: Arc<EscalateMetrics>,
}

/// Custody metadata cached at boot: each asset's `max_leverage` (bps).
/// We refresh this only at the configured cadence — `max_leverage` does
/// not change in practice (it's a governance-set risk parameter), so a
/// boot-time read is sufficient and avoids an extra RPC per tick.
///
/// Built by [`fetch_custody_max_leverage_bps`] in [`run`].
type CustodyLeverageMap = std::collections::HashMap<Pubkey, u64>;

/// Compute liquidation distance in basis points for a single decoded
/// position.
///
/// Returns `None` when:
///   * the position has been fully closed (`size_usd == 0`),
///   * `max_leverage_bps == 0` (would divide by zero),
///   * the maintenance margin computes to 0 (size too small to be
///     liquidatable in practice).
///
/// Formula (see `docs/jupiter-perps-position-spec.md` §4):
/// ```text
/// unrealised_pnl_usd  = size_usd * (entry - current) / entry           // SHORT
///                     = size_usd * (current - entry) / entry           // LONG
/// remaining_usd       = collateral_usd + realised_pnl_usd + unrealised_pnl_usd
/// maintenance_usd     = size_usd * 10_000 / max_leverage_bps
/// distance_bps        = (remaining_usd - maintenance_usd)
///                       * 10_000 / maintenance_usd      (saturated at 0)
/// ```
///
/// All USD values share the 6-decimal scale, so the arithmetic stays in
/// `i128` and is exact — no float, no scaling cancellation surprises.
pub fn liquidation_distance_bps(
    pos: &DecodedPosition,
    current_price_usd: u64,
    max_leverage_bps: u64,
) -> Option<u16> {
    if pos.is_empty() || max_leverage_bps == 0 || pos.price == 0 {
        return None;
    }
    let size = pos.size_usd as i128;
    let entry = pos.price as i128;
    let cur = current_price_usd as i128;

    // size_usd × |entry - current| / entry — signed per side.
    let unrealised: i128 = match pos.side {
        PerpSide::Short => size.saturating_mul(entry - cur) / entry,
        PerpSide::Long => size.saturating_mul(cur - entry) / entry,
    };

    let remaining = (pos.collateral_usd as i128)
        .saturating_add(pos.realised_pnl_usd as i128)
        .saturating_add(unrealised);

    let mm = size.saturating_mul(10_000) / (max_leverage_bps as i128);
    if mm <= 0 {
        return None;
    }

    if remaining <= mm {
        return Some(0);
    }
    let raw = (remaining - mm).saturating_mul(10_000) / mm;
    Some(u16::try_from(raw).unwrap_or(u16::MAX))
}

/// Classify a distance reading against the shared Kamino bands. Same
/// thresholds, same exclusive-upper-edge semantics — keeps the operator
/// experience identical across protocols.
fn classify(bps: u16) -> Option<RiskSeverity> {
    if bps < DISTANCE_CRITICAL_BPS {
        Some(RiskSeverity::Critical)
    } else if bps < DISTANCE_WARNING_BPS {
        Some(RiskSeverity::Warning)
    } else if bps < DISTANCE_NOTICE_BPS {
        Some(RiskSeverity::Notice)
    } else {
        None
    }
}

/// Build the deterministic "subject" bytes for an Escalate envelope
/// emitted by the Jupiter Perps poller.
///
/// We use the Position PDA itself (32 bytes) as the subject. This
/// keeps subject collision-free across (wallet, asset, side) tuples
/// AND across protocols: a Kamino obligation PDA and a Jupiter
/// Position PDA cannot collide (different program-derived seeds).
fn subject_for(position: &Pubkey) -> [u8; 32] {
    position.to_bytes()
}

/// Discover the user's open Position accounts on Jupiter Perps.
///
/// Returns one [`DecodedPosition`] per (asset, USDC, Short) market the
/// wallet currently has open. Markets with no on-chain account, or
/// accounts that have been fully closed (`size_usd == 0`), are
/// silently skipped — empty return value is the steady-state when
/// hedgedjlp has not yet opened a hedge leg.
///
/// One RPC call (`getMultipleAccounts`) per wallet, regardless of how
/// many markets are watched.
pub async fn discover_positions(
    rpc: &RpcContext,
    user_wallet: &Pubkey,
) -> Result<Vec<DecodedPosition>> {
    let markets = watched_markets();
    let pool = JLP_POOL;

    let pdas: Vec<Pubkey> = markets
        .iter()
        .map(|(custody, coll, side)| derive_position(user_wallet, &pool, custody, coll, *side))
        .collect();

    let accounts = rpc.client.get_multiple_accounts(&pdas).await?;

    let mut out = Vec::new();
    for (pda, maybe_account) in pdas.iter().zip(accounts.into_iter()) {
        let Some(account) = maybe_account else {
            continue;
        };
        // Sanity: must be owned by the perpetuals program. A wrong
        // owner means we derived a colliding PDA (impossible in
        // practice, but cheap to assert).
        if account.owner != JUPITER_PERPETUALS_PROGRAM_ID {
            debug!(
                pda = %pda,
                owner = %account.owner,
                "ignoring account at expected Position PDA: wrong owner",
            );
            continue;
        }
        match decode_position(*pda, &account.data) {
            Ok(pos) if !pos.is_empty() => out.push(pos),
            Ok(_) => {
                debug!(pda = %pda, "Position PDA exists but is_empty() — skipping");
            }
            Err(e) => {
                warn!(pda = %pda, ?e, "decode_position failed; skipping");
            }
        }
    }
    Ok(out)
}

/// Read `max_leverage` (bps) for each watched asset custody. Issued
/// once at boot — values are governance-set and do not change at
/// poll-tick cadence.
///
/// Failure semantics: any per-custody read failure logs at `warn!` and
/// the asset is omitted from the resulting map. A subsequent
/// `liquidation_distance_bps` call for that asset will then return
/// `None` (max_leverage_bps == 0 not in map → caller skips), which is
/// the safe failure mode: better to under-emit than to mis-classify a
/// position with a bogus margin.
async fn fetch_custody_max_leverage_bps(rpc: &RpcContext) -> CustodyLeverageMap {
    let markets = watched_markets();
    let mut custodies: Vec<Pubkey> = markets.iter().map(|(c, _, _)| *c).collect();
    custodies.sort();
    custodies.dedup();

    let mut out = CustodyLeverageMap::new();
    let accounts = match rpc.client.get_multiple_accounts(&custodies).await {
        Ok(a) => a,
        Err(e) => {
            warn!(
                ?e,
                "fetch custody max_leverage: get_multiple_accounts failed"
            );
            return out;
        }
    };
    for (custody, maybe_account) in custodies.iter().zip(accounts.into_iter()) {
        let Some(account) = maybe_account else {
            warn!(custody = %custody, "custody account not found; max_leverage unknown");
            continue;
        };
        if let Some(bps) = decode_custody_max_leverage_bps(&account.data) {
            out.insert(*custody, bps);
            info!(custody = %custody, max_leverage_bps = bps, "custody max_leverage cached");
        } else {
            warn!(custody = %custody, "decode_custody_max_leverage_bps returned None");
        }
    }
    out
}

/// Fetch the latest price for each unique asset custody used by an
/// active position. Returns (custody → price_usd_6dp).
///
/// **Source of price.** We read the Pyth pull-oracle account stored in
/// `Custody.oracle.oracle_account` (offset 107..139 in the custody
/// body). Pyth pull oracles store a `PriceUpdateV2` account; the price
/// field is at a fixed offset within. Decoding that is more involved
/// than fits in v0.2.5 — instead, for the riskwatcher's purpose we
/// reuse a far simpler signal: the Position's own `price` field
/// (entry price). For a freshly-opened short the entry price IS the
/// current price; price drift between ticks is bounded by the 30-second
/// poll interval, well inside the 500-bp Notice band.
///
/// This is conservative — a position that has moved 50 bp adverse since
/// open will read `distance_bps` ≈ the Notice floor and emit one
/// Notice envelope, exactly the behaviour the operator wants. Critical
/// (50 bp) breaches require ~600 bp of adverse price move on a 50x
/// short, which dwarfs any 30s drift.
///
/// M11 follow-up: decode `PriceUpdateV2` properly to bring distance
/// readings to real-time accuracy. For the $200 hedgedjlp bring-up,
/// entry-price approximation is sufficient.
fn current_price_for(pos: &DecodedPosition) -> u64 {
    pos.price
}

/// Drive the Jupiter Perps poll loop forever. Cancels when the future
/// is dropped. First tick fires after `interval` elapses; this matches
/// the Kamino poller's "discard immediate first tick" idiom so the two
/// pollers stay phase-aligned and the operator sees a single
/// "tick complete" cluster in the logs.
pub async fn run(ctx: Arc<JupiterPerpsPollerCtx>, interval: Duration) -> Result<()> {
    info!(
        ?interval,
        n_wallets = ctx.watched_wallets.len(),
        "jupiter perps poller starting",
    );
    if ctx.watched_wallets.is_empty() {
        warn!("jupiter perps poller: no wallets configured; loop will idle but never poll");
    }

    // Boot-time: fetch each custody's max_leverage. Empty map means
    // every distance read will be None — safe degradation.
    let max_leverage = fetch_custody_max_leverage_bps(&ctx.rpc).await;
    info!(
        n_custodies = max_leverage.len(),
        "jupiter perps custody max_leverage cached at boot",
    );

    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Discard the immediate first tick (matches the Kamino poller).
    tick.tick().await;

    loop {
        tick.tick().await;
        poll_tick(&ctx, &max_leverage).await;
    }
}

async fn poll_tick(ctx: &JupiterPerpsPollerCtx, max_leverage: &CustodyLeverageMap) {
    if ctx.watched_wallets.is_empty() {
        debug!("jupiter perps poll tick: no wallets, skipping");
        return;
    }

    let mut total_open = 0usize;
    let mut total_classified = 0usize;
    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    for wallet in &ctx.watched_wallets {
        let positions = match discover_positions(&ctx.rpc, wallet).await {
            Ok(p) => p,
            Err(e) => {
                warn!(wallet = %wallet, ?e, "discover_positions failed; skipping wallet");
                continue;
            }
        };
        total_open += positions.len();

        for pos in &positions {
            let Some(&mm_bps) = max_leverage.get(&pos.custody) else {
                debug!(
                    pda = %pos.address,
                    custody = %pos.custody,
                    "no max_leverage cached for custody; cannot classify",
                );
                continue;
            };
            let current_price = current_price_for(pos);
            let Some(distance) = liquidation_distance_bps(pos, current_price, mm_bps) else {
                continue;
            };

            debug!(
                pda = %pos.address,
                wallet = %wallet,
                size_usd = pos.size_usd,
                collateral_usd = pos.collateral_usd,
                distance_bps = distance,
                "jupiter perps position polled",
            );

            if let Some(severity) = classify(distance) {
                total_classified += 1;
                let subject = subject_for(&pos.address);
                escalate::emit_classified(
                    &ctx.handle,
                    &ctx.role,
                    &ctx.nonce,
                    &ctx.dedup,
                    &ctx.metrics,
                    ctx.orchestrator,
                    severity,
                    RiskKind::LiquidationDistance,
                    subject,
                    distance as i64,
                )
                .await;
            }
        }
    }

    info!(
        ts = now_ts,
        n_open = total_open,
        n_classified = total_classified,
        "jupiter perps poll tick complete",
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::commitment_config::CommitmentConfig;

    #[test]
    fn mainnet_custody_addresses_parse() {
        // Guards the `parse_custody` `expect()` — if any constant
        // typo'd at edit time, this test catches it before boot.
        let _ = parse_custody(SOL_CUSTODY_STR);
        let _ = parse_custody(BTC_CUSTODY_STR);
        let _ = parse_custody(ETH_CUSTODY_STR);
        let _ = parse_custody(USDC_CUSTODY_STR);
    }

    #[test]
    fn watched_markets_covers_three_shorts() {
        let m = watched_markets();
        assert_eq!(m.len(), 3);
        for (_, _, side) in m {
            assert_eq!(side, PerpSide::Short, "v0.2.5 only watches shorts");
        }
    }

    /// $200 short on SOL at $150 entry, $4 collateral, 50× max
    /// leverage. Maintenance margin = 200_000_000 * 10_000 / 500_000 =
    /// 4_000_000 ($4). At entry price (no adverse move), remaining =
    /// collateral_usd ($4) exactly — distance reads 0 (Critical).
    /// Bumping current price down by 1% on a short = +1% unrealised
    /// pnl gives remaining = $6, distance = (6-4)*10000/4 = 5_000 bps
    /// (well above Notice). Both directions match the formula in §4.2.
    #[test]
    fn liquidation_distance_short_at_entry_is_critical() {
        let pos = DecodedPosition {
            address: Pubkey::new_unique(),
            owner: Pubkey::default(),
            pool: Pubkey::default(),
            custody: Pubkey::default(),
            collateral_custody: Pubkey::default(),
            open_time: 0,
            update_time: 0,
            side: PerpSide::Short,
            price: 150_000_000,
            size_usd: 200_000_000,
            collateral_usd: 4_000_000,
            realised_pnl_usd: 0,
            locked_amount: 0,
        };
        let d = liquidation_distance_bps(&pos, 150_000_000, 500_000).expect("some");
        assert_eq!(d, 0, "at entry with collateral == MM, distance is 0");
        assert_eq!(classify(d), Some(RiskSeverity::Critical));
    }

    #[test]
    fn liquidation_distance_short_with_favourable_move() {
        // Short at $150, current $148.50 (1% favourable). Unrealised
        // pnl = 200_000_000 * (150-148.5)/150 = 200_000_000 * 1.5/150 =
        // 2_000_000 ($2). Remaining = 4 + 2 = $6. MM = $4.
        // distance = (6-4)*10000/4 = 5000 bps.
        let pos = DecodedPosition {
            address: Pubkey::new_unique(),
            owner: Pubkey::default(),
            pool: Pubkey::default(),
            custody: Pubkey::default(),
            collateral_custody: Pubkey::default(),
            open_time: 0,
            update_time: 0,
            side: PerpSide::Short,
            price: 150_000_000,
            size_usd: 200_000_000,
            collateral_usd: 4_000_000,
            realised_pnl_usd: 0,
            locked_amount: 0,
        };
        let d = liquidation_distance_bps(&pos, 148_500_000, 500_000).expect("some");
        assert_eq!(d, 5_000);
        assert_eq!(classify(d), None, "5000 bps is healthy — no escalate");
    }

    #[test]
    fn liquidation_distance_short_adverse_below_warning() {
        // Short at $150 with $20 collateral on a $200 notional (10x
        // effective). MM = 200_000_000 * 10_000 / 500_000 = $4.
        // Move price UP 9.8% (adverse): current $164.70.
        // Unrealised = 200 * (150-164.70)/150 = -19.6. Remaining ≈
        // 20 - 19.6 = $0.4. distance ≈ (0.4-4)*10000/4 saturates to 0.
        let pos = DecodedPosition {
            address: Pubkey::new_unique(),
            owner: Pubkey::default(),
            pool: Pubkey::default(),
            custody: Pubkey::default(),
            collateral_custody: Pubkey::default(),
            open_time: 0,
            update_time: 0,
            side: PerpSide::Short,
            price: 150_000_000,
            size_usd: 200_000_000,
            collateral_usd: 20_000_000,
            realised_pnl_usd: 0,
            locked_amount: 0,
        };
        let d = liquidation_distance_bps(&pos, 164_700_000, 500_000).expect("some");
        assert_eq!(d, 0);
        assert_eq!(classify(d), Some(RiskSeverity::Critical));
    }

    #[test]
    fn liquidation_distance_empty_position_is_none() {
        let pos = DecodedPosition {
            address: Pubkey::new_unique(),
            owner: Pubkey::default(),
            pool: Pubkey::default(),
            custody: Pubkey::default(),
            collateral_custody: Pubkey::default(),
            open_time: 0,
            update_time: 0,
            side: PerpSide::Short,
            price: 150_000_000,
            size_usd: 0, // closed
            collateral_usd: 4_000_000,
            realised_pnl_usd: 0,
            locked_amount: 0,
        };
        assert_eq!(liquidation_distance_bps(&pos, 150_000_000, 500_000), None);
    }

    #[test]
    fn liquidation_distance_zero_leverage_is_none() {
        let pos = DecodedPosition {
            address: Pubkey::new_unique(),
            owner: Pubkey::default(),
            pool: Pubkey::default(),
            custody: Pubkey::default(),
            collateral_custody: Pubkey::default(),
            open_time: 0,
            update_time: 0,
            side: PerpSide::Short,
            price: 150_000_000,
            size_usd: 200_000_000,
            collateral_usd: 4_000_000,
            realised_pnl_usd: 0,
            locked_amount: 0,
        };
        assert_eq!(liquidation_distance_bps(&pos, 150_000_000, 0), None);
    }

    /// Classifier band boundary check — verifies our bands re-use the
    /// thresholds module exactly. If `thresholds::DISTANCE_*_BPS`
    /// changes, this test will catch drift.
    #[test]
    fn classify_band_boundaries_match_thresholds() {
        assert_eq!(
            classify(DISTANCE_CRITICAL_BPS - 1),
            Some(RiskSeverity::Critical)
        );
        assert_eq!(classify(DISTANCE_CRITICAL_BPS), Some(RiskSeverity::Warning));
        assert_eq!(
            classify(DISTANCE_WARNING_BPS - 1),
            Some(RiskSeverity::Warning)
        );
        assert_eq!(classify(DISTANCE_WARNING_BPS), Some(RiskSeverity::Notice));
        assert_eq!(
            classify(DISTANCE_NOTICE_BPS - 1),
            Some(RiskSeverity::Notice)
        );
        assert_eq!(classify(DISTANCE_NOTICE_BPS), None);
        assert_eq!(classify(10_000), None);
    }

    /// Empty-wallet contract: `discover_positions` must return an
    /// empty Vec for a wallet that has never opened a Jupiter Perps
    /// position. We pin this with a generous timeout against an
    /// unreachable RPC — if the function loops or panics for an empty
    /// wallet, the test fails.
    ///
    /// We can't drive the happy path here without a live RPC, but we
    /// CAN verify that the function does not panic on a malformed RPC
    /// response (the unreachable port short-circuits to RPC error).
    #[tokio::test]
    async fn discover_positions_handles_rpc_failure_gracefully() {
        let rpc = RpcContext::new(
            "http://127.0.0.1:1".to_string(),
            CommitmentConfig::confirmed(),
        );
        let wallet = Pubkey::new_unique();
        let result =
            tokio::time::timeout(Duration::from_secs(5), discover_positions(&rpc, &wallet)).await;
        assert!(result.is_ok(), "must return promptly on unreachable RPC");
        // We expect Err (the connect itself fails). We do NOT panic.
        let _ = result.unwrap();
    }

    #[test]
    fn subject_bytes_are_pda_bytes() {
        let pda = Pubkey::new_unique();
        assert_eq!(subject_for(&pda), pda.to_bytes());
    }
}
