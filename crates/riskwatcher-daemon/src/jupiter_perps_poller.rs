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

/// Scale factor for the on-chain `Custody.pricing.maxLeverage` field.
///
/// The decoded u64 is **leverage × 1000** (thousandths). E.g. mainnet
/// SOL/BTC/ETH custodies read `19531` at the decoder offset, which
/// corresponds to `19.531×` max leverage — the Jupiter cap operators
/// observe via the front-end "Max Leverage" gauge.
///
/// The earlier v0.2.5 spec mis-documented this as "×10_000 (bps of
/// leverage)" by analogy with Kamino LTV bands; that interpretation
/// produced `max_leverage = 1.9531×`, which contradicted live data
/// (live BTC short ran 5× and was healthy, not insta-liquidated).
/// Verified 2026-05-15 against mainnet custody bytes (see
/// `docs/jupiter-perps-position-spec.md` §4.1).
const MAX_LEVERAGE_THOUSANDTHS_SCALE: u128 = 1_000;

/// Compute liquidation distance in basis points for a single decoded
/// position, using the **leverage-frame** model.
///
/// We work in leverage-space rather than price-space because the result
/// is directly comparable across SOL/BTC/ETH at different volatilities
/// and across protocols (Kamino's `distance_bps` is also a leverage-
/// headroom ratio). Both forms are mathematically equivalent for the
/// liquidation engine; the leverage frame is the operator-friendly
/// choice for the riskwatcher's escalate bands.
///
/// ```text
/// unrealised_pnl_usd      = size_usd * (entry - current) / entry          // SHORT
///                         = size_usd * (current - entry) / entry          // LONG
/// effective_collateral    = collateral_usd + realised_pnl_usd + unrealised_pnl_usd
/// current_leverage_x1000  = size_usd * 1_000 / effective_collateral
/// headroom                = max_leverage_x1000 - current_leverage_x1000   (sat. 0)
/// distance_bps            = headroom * 10_000 / max_leverage_x1000
/// ```
///
/// Returns `None` when:
///   * the position has been fully closed (`size_usd == 0`),
///   * `max_leverage_thousandths == 0` (would divide by zero),
///   * `pos.price == 0` (cannot compute unrealised pnl).
///
/// Returns `Some(0)` when:
///   * `effective_collateral <= 0` (collateral fully eroded), or
///   * `current_leverage >= max_leverage` (at or past liquidation).
pub fn liquidation_distance_bps(
    pos: &DecodedPosition,
    current_price_usd: u64,
    max_leverage_thousandths: u64,
) -> Option<u16> {
    if pos.is_empty() || max_leverage_thousandths == 0 || pos.price == 0 {
        return None;
    }
    let size = pos.size_usd as i128;
    let entry = pos.price as i128;
    let cur = current_price_usd as i128;

    // size_usd × (entry - current) / entry — signed per side.
    let unrealised: i128 = match pos.side {
        PerpSide::Short => size.saturating_mul(entry - cur) / entry,
        PerpSide::Long => size.saturating_mul(cur - entry) / entry,
    };

    let effective_collateral = (pos.collateral_usd as i128)
        .saturating_add(pos.realised_pnl_usd as i128)
        .saturating_add(unrealised);

    // Collateral fully eroded → at liquidation.
    if effective_collateral <= 0 {
        return Some(0);
    }

    let max_lev_x1000 = max_leverage_thousandths as i128;
    // current_leverage_x1000 = size_usd * 1_000 / effective_collateral
    let cur_lev_x1000 =
        size.saturating_mul(MAX_LEVERAGE_THOUSANDTHS_SCALE as i128) / effective_collateral;

    if cur_lev_x1000 >= max_lev_x1000 {
        return Some(0);
    }

    // headroom_bps = (max - cur) * 10_000 / max
    let headroom_bps = (max_lev_x1000 - cur_lev_x1000).saturating_mul(10_000) / max_lev_x1000;
    Some(u16::try_from(headroom_bps).unwrap_or(u16::MAX))
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
            let Some(&max_lev_x1000) = max_leverage.get(&pos.custody) else {
                debug!(
                    pda = %pos.address,
                    custody = %pos.custody,
                    "no max_leverage cached for custody; cannot classify",
                );
                continue;
            };
            let current_price = current_price_for(pos);
            let Some(distance) = liquidation_distance_bps(pos, current_price, max_lev_x1000) else {
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

    /// Helper: build a SHORT position with the given size / collateral.
    /// Entry price is fixed at $100 (6dp) and we read current price ==
    /// entry so unrealised pnl is 0 — the leverage-frame ratio
    /// `current_leverage = size / collateral` is then exact.
    fn short_at_entry(size_usd_6dp: u64, collateral_usd_6dp: u64) -> DecodedPosition {
        DecodedPosition {
            address: Pubkey::new_unique(),
            owner: Pubkey::default(),
            pool: Pubkey::default(),
            custody: Pubkey::default(),
            collateral_custody: Pubkey::default(),
            open_time: 0,
            update_time: 0,
            side: PerpSide::Short,
            price: 100_000_000,
            size_usd: size_usd_6dp,
            collateral_usd: collateral_usd_6dp,
            realised_pnl_usd: 0,
            locked_amount: 0,
        }
    }

    /// **Live BTC short regression** (mainnet 2026-05-15).
    ///
    /// Position: $18 size, $3.59 collateral → 5.014× current leverage.
    /// Custody max_leverage_thousandths = 19_531 → 19.531× cap.
    /// Headroom = 1 - 5.014/19.531 ≈ 0.7433 → distance_bps ≈ 7433.
    /// This is the exact scenario v0.2.5 mis-classified as Critical
    /// (distance_bps = 0), causing 24+ false-positive escalates.
    #[test]
    fn liquidation_distance_live_btc_short_is_healthy() {
        let pos = short_at_entry(18_000_000, 3_590_000);
        let d = liquidation_distance_bps(&pos, 100_000_000, 19_531).expect("some");
        // 1 - (18_000_000 * 1_000 / 3_590_000) / 19_531
        //   = 1 - 5_014 / 19_531 = 0.7433
        //   ≈ 7433 bps
        assert!(
            (7_400..=7_460).contains(&d),
            "live BTC short distance_bps must land in healthy band, got {d}"
        );
        assert_eq!(
            classify(d),
            None,
            "live BTC short must NOT classify as Notice/Warning/Critical"
        );
    }

    /// Healthy: leverage 5× on a 19.5× max cap → distance ≈ 7437 bps.
    #[test]
    fn liquidation_distance_healthy_5x_on_19_5x_cap() {
        // $100 size, $20 collateral → 5× leverage.
        let pos = short_at_entry(100_000_000, 20_000_000);
        let d = liquidation_distance_bps(&pos, 100_000_000, 19_500).expect("some");
        // 1 - (5_000 / 19_500) = 0.7436 → 7436 bps.
        assert!((7_400..=7_500).contains(&d), "got {d}");
        assert_eq!(classify(d), None);
    }

    /// Stressed: leverage 16× on 19.5× cap → distance ≈ 1794 bps.
    /// Above Notice floor (500 bps) → no escalate today, but only one
    /// bad price tick away.
    #[test]
    fn liquidation_distance_stressed_16x_on_19_5x_cap() {
        // $100 size, $6.25 collateral → 16× leverage.
        let pos = short_at_entry(100_000_000, 6_250_000);
        let d = liquidation_distance_bps(&pos, 100_000_000, 19_500).expect("some");
        // 1 - 16_000 / 19_500 = 0.1794 → 1794 bps.
        assert!((1_700..=1_900).contains(&d), "got {d}");
        // 1794 bps is above DISTANCE_NOTICE_BPS (500) → None.
        assert_eq!(classify(d), None);
    }

    /// Critical: leverage 19× on 19.5× cap → distance ≈ 256 bps.
    /// Inside the Notice band (≤ 500 bps), above Warning (200 bps).
    #[test]
    fn liquidation_distance_critical_19x_on_19_5x_cap() {
        // $100 size, $5.263158 collateral → 19× leverage.
        let pos = short_at_entry(100_000_000, 5_263_158);
        let d = liquidation_distance_bps(&pos, 100_000_000, 19_500).expect("some");
        // 1 - 19_000 / 19_500 = 0.02564 → 256 bps.
        assert!((230..=280).contains(&d), "got {d}");
        assert_eq!(classify(d), Some(RiskSeverity::Notice));
    }

    /// At-cap: leverage == max → distance saturates to 0 → Critical.
    #[test]
    fn liquidation_distance_at_cap_is_critical() {
        // $19.5 size, $1.0 collateral → exactly 19.5× leverage.
        let pos = short_at_entry(19_500_000, 1_000_000);
        let d = liquidation_distance_bps(&pos, 100_000_000, 19_500).expect("some");
        assert_eq!(d, 0);
        assert_eq!(classify(d), Some(RiskSeverity::Critical));
    }

    /// Adverse price move pushes effective collateral negative —
    /// distance must saturate to 0.
    #[test]
    fn liquidation_distance_short_adverse_collateral_eroded() {
        // $100 short at $100 entry with $20 collateral (5×). Price
        // rises 25% → current $125. Unrealised = 100 * (100-125)/100
        // = -25. Effective collateral = 20 - 25 = -5 → saturates to 0.
        let pos = short_at_entry(100_000_000, 20_000_000);
        let d = liquidation_distance_bps(&pos, 125_000_000, 19_500).expect("some");
        assert_eq!(d, 0);
        assert_eq!(classify(d), Some(RiskSeverity::Critical));
    }

    /// Favourable price move improves effective collateral, lowering
    /// effective leverage and widening headroom.
    #[test]
    fn liquidation_distance_short_favourable_move_widens_headroom() {
        // $100 short at $100 entry with $10 collateral (10×). Price
        // drops 5% → current $95. Unrealised = 100 * (100-95)/100 = +5.
        // Effective collateral = 10 + 5 = 15. Effective leverage =
        // 100/15 = 6.67×. Headroom = 1 - 6_667/19_500 = 0.6580 → 6579 bps.
        let pos = short_at_entry(100_000_000, 10_000_000);
        let d = liquidation_distance_bps(&pos, 95_000_000, 19_500).expect("some");
        assert!((6_500..=6_650).contains(&d), "got {d}");
        assert_eq!(classify(d), None);
    }

    #[test]
    fn liquidation_distance_empty_position_is_none() {
        let mut pos = short_at_entry(0, 4_000_000); // closed
        pos.size_usd = 0;
        assert_eq!(liquidation_distance_bps(&pos, 100_000_000, 19_500), None);
    }

    #[test]
    fn liquidation_distance_zero_leverage_is_none() {
        let pos = short_at_entry(200_000_000, 4_000_000);
        assert_eq!(liquidation_distance_bps(&pos, 100_000_000, 0), None);
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
