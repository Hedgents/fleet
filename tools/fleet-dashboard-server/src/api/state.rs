//! `GET /aum`, `GET /pnl`, `GET /positions`, `GET /daemons`,
//! `GET /apr/history`.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::warn;

use crate::api::AppState;
use zerox1_defi_protocols::constants::{KAMINO_MAIN_MARKET, KAMINO_MAIN_USDC_RESERVE};

const DAEMON_ROLES: &[&str] = &[
    "multiply",
    "stable_yield",
    "hedgedjlp",
    "riskwatcher",
    "researcher",
    "orchestrator",
];

const STATUS_GREEN_MS: i64 = 30_000;
const STATUS_YELLOW_MS: i64 = 120_000;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/aum", get(aum))
        .route("/pnl", get(pnl))
        .route("/paper", get(paper_trading))
        .route("/positions", get(positions))
        .route("/daemons", get(daemons))
        .route("/wallet", get(wallet))
        .route("/rates", get(rates_handler))
        .route("/strategies", get(strategies))
        .route("/apr/history", get(apr_history))
        .route("/orchestrator/decisions", get(orchestrator_decisions))
        .route("/lifetime", get(lifetime))
}

// ── /lifetime ────────────────────────────────────────────────────────────────
//
// Time-on-mainnet metrics for the dashboard's hero banner. The product
// thesis ("operational reliability earns trust over time") only compounds
// as a moat if prospects can see the time accumulating — otherwise it's
// invisible to anyone who doesn't read the DEVLOG. This endpoint surfaces
// the three numbers that matter: when did we go live, how long has the
// fleet been running, and how many real-incidents-with-regression-tests
// have we accumulated.

/// Genesis of the live mainnet reference deployment: 2026-05-09T00:00:00Z.
/// First production tx landed at 2026-05-09T20:55:18Z (multiply lever-up,
/// signature 4R6pJeH5...). The midnight UTC of the same day is the
/// canonical "live since" date — operators rounded their first tx to
/// "May 9" and we honour the rounding.
///
/// rc20 fix: rc19 shipped with `1_746_748_800` (= 2025-05-09), one year
/// off, which made the dashboard banner report 376 days of uptime. The
/// `lifetime_constants_match_devlog` test pinned the wrong value too,
/// so the regression slipped past CI — the test catches "constant
/// drifted from itself" but not "constant was wrong on day one." Fixed
/// both. Lesson: when a constant encodes a real-world date, it's worth
/// asserting against a derived value (year ≥ 2026) rather than a literal.
const LIVE_SINCE_UNIX: i64 = 1_778_284_800;

/// Count of release-candidate incidents documented in DEVLOG.md with a
/// regression test. Bumped manually per release tag so a new rc lands
/// in the hero banner the moment its commit ships. Kept here rather
/// than parsed from DEVLOG to avoid a build-time dep on the markdown
/// file format — and to make the value impossible to inflate by
/// reformatting the changelog.
const INCIDENTS_RESOLVED: u32 = 18;

#[derive(Serialize)]
struct LifetimeOut {
    /// Unix seconds. Frontend renders "Live since {ISO date}".
    live_since_unix: i64,
    /// Server time at response. Lets the frontend animate an uptime
    /// counter that ticks every second without an extra round-trip.
    now_unix: i64,
    /// Convenience: seconds the fleet has been live. Equivalent to
    /// `now_unix - live_since_unix` but pre-computed for clients that
    /// don't want to do the subtraction in JS.
    uptime_secs: i64,
    /// Total number of rc-tagged incidents documented in DEVLOG.md with
    /// a regression test pinning the failure shape.
    incidents_resolved: u32,
}

async fn lifetime(State(_state): State<AppState>) -> impl IntoResponse {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(LIVE_SINCE_UNIX);
    let uptime_secs = (now_unix - LIVE_SINCE_UNIX).max(0);
    Json(LifetimeOut {
        live_since_unix: LIVE_SINCE_UNIX,
        now_unix,
        uptime_secs,
        incidents_resolved: INCIDENTS_RESOLVED,
    })
}

// ── /orchestrator/decisions ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OrchestratorDecisionsQuery {
    /// Max records to return, newest first. Default 20, cap 200.
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct OrchestratorDecisionRow {
    ts_unix: u64,
    mode: String,
    action: String,
    reason: String,
    /// `"sent"`, `"failed:<reason>"`, `"skipped:<reason>"`, or empty.
    envelope_result: String,
    /// Where the action would land (e.g. `"stable_yield"`). `None` for NoAction.
    strategy: Option<String>,
    /// USD amount. 0.0 for NoAction.
    amount_usd: f64,
}

async fn orchestrator_decisions(
    axum::extract::Query(q): axum::extract::Query<OrchestratorDecisionsQuery>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(20).min(200);
    let path = state.telemetry_dir.join("orchestrator-audit.jsonl");

    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return axum::Json(Vec::<OrchestratorDecisionRow>::new()),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => return axum::Json(Vec::<OrchestratorDecisionRow>::new()),
    };

    let mut out: Vec<OrchestratorDecisionRow> = text
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take(limit)
        .filter_map(parse_decision_line)
        .collect();
    // Newest first is what the caller wants — but tail-then-take already
    // gives that ordering since we reversed lines before take.
    out.truncate(limit);
    axum::Json(out)
}

fn parse_decision_line(line: &str) -> Option<OrchestratorDecisionRow> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ts_unix = v.get("ts_unix")?.as_u64()?;
    let mode = v.get("mode")?.as_str()?.to_string();
    let action_obj = v.get("action")?;
    let action = action_obj.get("action")?.as_str()?.to_string();
    let reason = action_obj
        .get("reason")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let envelope_result = v
        .get("envelope_result")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let strategy = action_obj
        .get("strategy")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let amount_usd = action_obj
        .get("amount_usd")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    Some(OrchestratorDecisionRow {
        ts_unix,
        mode,
        action,
        reason,
        envelope_result,
        strategy,
        amount_usd,
    })
}

// ── /apr/history ─────────────────────────────────────────────────────────────

const APR_HISTORY_DEFAULT_HOURS: u32 = 24;
const APR_HISTORY_MAX_HOURS: u32 = 168; // 1 week
const APR_HISTORY_CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct AprHistoryQuery {
    strategy: Option<String>,
    hours: Option<u32>,
}

#[derive(Clone, Serialize)]
struct AprSample {
    ts_ms: i64,
    apr_bps: i64,
}

#[derive(Clone, Serialize)]
struct AprStats {
    min_bps: i64,
    max_bps: i64,
    mean_bps: i64,
    p50_bps: i64,
    samples_count: u64,
}

#[derive(Clone, Serialize)]
struct AprHistoryOut {
    strategy: String,
    hours: u32,
    samples: Vec<AprSample>,
    stats: AprStats,
}

/// Cache key = (strategy, hours).
type AprCacheKey = (String, u32);
#[allow(clippy::type_complexity)]
static APR_HISTORY_CACHE: OnceLock<
    Mutex<std::collections::HashMap<AprCacheKey, (Instant, AprHistoryOut)>>,
> = OnceLock::new();

fn compute_stats(samples: &[AprSample]) -> AprStats {
    let n = samples.len();
    if n == 0 {
        return AprStats {
            min_bps: 0,
            max_bps: 0,
            mean_bps: 0,
            p50_bps: 0,
            samples_count: 0,
        };
    }
    let mut vals: Vec<i64> = samples.iter().map(|s| s.apr_bps).collect();
    vals.sort_unstable();
    let min_bps = *vals.first().unwrap();
    let max_bps = *vals.last().unwrap();
    let sum: i128 = vals.iter().map(|x| *x as i128).sum();
    let mean_bps = (sum / n as i128) as i64;
    // p50 = lower median (n/2 index after sort), matches the documented
    // sample output for institutional dashboards (no interpolation).
    let p50_bps = vals[n / 2];
    AprStats {
        min_bps,
        max_bps,
        mean_bps,
        p50_bps,
        samples_count: n as u64,
    }
}

async fn apr_history(
    State(state): State<AppState>,
    Query(q): Query<AprHistoryQuery>,
) -> impl IntoResponse {
    let Some(strategy) = q.strategy else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing required ?strategy="})),
        )
            .into_response();
    };
    let hours = q
        .hours
        .unwrap_or(APR_HISTORY_DEFAULT_HOURS)
        .clamp(1, APR_HISTORY_MAX_HOURS);

    let cache = APR_HISTORY_CACHE.get_or_init(|| Mutex::new(Default::default()));
    let key = (strategy.clone(), hours);
    {
        let guard = cache.lock().await;
        if let Some((ts, cached)) = guard.get(&key) {
            if ts.elapsed() < APR_HISTORY_CACHE_TTL {
                return Json(cached.clone()).into_response();
            }
        }
    }

    let rows = state
        .store
        .apr_samples_for(&strategy, hours)
        .await
        .unwrap_or_default();
    let samples: Vec<AprSample> = rows
        .into_iter()
        .map(|(ts_ms, apr_bps)| AprSample { ts_ms, apr_bps })
        .collect();
    let stats = compute_stats(&samples);
    let out = AprHistoryOut {
        strategy: strategy.clone(),
        hours,
        samples,
        stats,
    };

    {
        let mut guard = cache.lock().await;
        guard.insert(key, (Instant::now(), out.clone()));
    }
    Json(out).into_response()
}

async fn rates_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.chain.rate_snapshot().await)
}

#[derive(Serialize)]
struct WalletOut {
    /// Base58-encoded operator pubkey. The fleet signs every tx with the
    /// keypair that derives to this address — fund this address to
    /// enable trading.
    pubkey: String,
    sol_lamports: u64,
    usdc_lamports: u64,
    jlp_lamports: u64,
    /// RPC URL the dashboard is talking to. Lets the frontend infer the
    /// network label (devnet/mainnet) without a separate config bridge.
    rpc_url: String,
}

async fn wallet(State(state): State<AppState>) -> impl IntoResponse {
    // Reuse the chain reader's 30s cache (same one /aum hits) so the
    // frontend can poll /wallet every 5s without hammering the RPC.
    let bal = state
        .chain
        .wallet_balances(&state.wallet_pubkey)
        .await
        .unwrap_or_default();
    Json(WalletOut {
        pubkey: state.wallet_pubkey.to_string(),
        sol_lamports: bal.sol_lamports,
        usdc_lamports: bal.usdc_lamports,
        jlp_lamports: bal.jlp_lamports,
        rpc_url: state.rpc_url.clone(),
    })
}

#[derive(Serialize)]
struct PerStrategy {
    multiply: f64,
    stable_yield: f64,
    /// Mark-to-market value of the JLP held in the hedgedjlp wallet.
    /// Excludes the USDC collateral funding the Jupiter Perps shorts —
    /// see `hedgedjlp_collateral_usd` for that.
    hedgedjlp_jlp_value_usd: f64,
    /// USDC sitting inside Jupiter Perps short positions as collateral
    /// (sum of `collateral_usd` across SOL+ETH+BTC shorts). This is real
    /// deployed capital that doesn't appear in the operator's wallet ATA
    /// but is recoverable on unwind. Added in rc16: the pre-rc16
    /// dashboard understated total AUM by exactly this amount because
    /// hedge collateral was orphaned between the wallet (drained) and
    /// the strategy line (JLP-value only).
    hedgedjlp_collateral_usd: f64,
    idle_usdc: f64,
}

#[derive(Serialize)]
struct AumOut {
    total_usdc: f64,
    per_strategy: PerStrategy,
    /// Deployed-USD-weighted average of per-strategy APRs (basis points).
    /// Idle USDC is excluded from both numerator and denominator since it
    /// earns 0% and would otherwise dilute the operational-yield figure.
    /// Zero when no capital is deployed.
    combined_apr_bps: u32,
    /// Projected yearly USD earnings at the current combined APR on the
    /// currently-deployed capital. Useful for institutional pitch: "earning
    /// $X/year on $Y deployed".
    combined_annualised_usd: f64,
}

/// Resolve the current APR (bps) for a strategy daemon, using the same
/// sourcing rules as the `/strategies` handler:
///   - `stable_yield` → Kamino USDC supply rate from on-chain rates snapshot
///   - `multiply` / `hedgedjlp` → newest pnl_snapshot row's apr field
pub(crate) async fn current_apr_bps_for(daemon: &str, state: &AppState) -> u32 {
    let meta = STRATEGIES.iter().find(|s| s.daemon == daemon);
    let Some(meta) = meta else { return 0 };
    if meta.id == "stable_yield" {
        let rates = state.chain.rate_snapshot().await;
        return rates.kamino_usdc_supply_bps;
    }
    let newest = state
        .store
        .recent_pnl_for(meta.daemon, 1)
        .await
        .ok()
        .and_then(|mut rows| rows.pop());
    match newest {
        None => 0,
        Some((_, json)) => {
            let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
            v.get(meta.apr_field).and_then(|x| x.as_u64()).unwrap_or(0) as u32
        }
    }
}

/// Weighted-average APR (bps) of strategies by deployed USD. Idle is excluded.
/// Returns 0 if total deployed USD is zero.
pub(crate) fn weighted_combined_apr_bps(weights: &[(f64, u32)]) -> u32 {
    let total: f64 = weights.iter().map(|(usd, _)| *usd).sum();
    if total <= 0.0 {
        return 0;
    }
    let weighted: f64 = weights.iter().map(|(usd, bps)| usd * (*bps as f64)).sum();
    (weighted / total).round() as u32
}

/// Per-strategy USD breakdown read straight from chain state. The
/// single source of truth for AUM math: used by both the /aum handler
/// (synchronous response) and the rc24 aum_sampler (writes to the
/// `chain_aum_snapshots` table). Daemon telemetry (which had drifted
/// out of sync with chain — see DEVLOG rc24) is no longer consulted.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChainAumBreakdown {
    pub multiply_usd: f64,
    pub stable_yield_usd: f64,
    pub hedgedjlp_jlp_usd: f64,
    pub hedgedjlp_collateral_usd: f64,
    pub idle_usd: f64,
}

impl ChainAumBreakdown {
    pub fn total_usd(&self) -> f64 {
        self.multiply_usd
            + self.stable_yield_usd
            + self.hedgedjlp_jlp_usd
            + self.hedgedjlp_collateral_usd
            + self.idle_usd
    }
}

/// Read chain-state AUM into a [`ChainAumBreakdown`]. All chain reads
/// are best-effort: a per-strategy RPC failure degrades to 0.0 for
/// that line rather than refusing the whole response. The /aum
/// handler and the rc24 sampler share this code path so /pnl and /aum
/// can never disagree by construction.
pub(crate) async fn read_chain_aum_breakdown(
    chain: &crate::chain::ChainReader,
    wallet: &solana_sdk::pubkey::Pubkey,
) -> ChainAumBreakdown {
    let balances = chain.wallet_balances(wallet).await.ok();
    let multiply = chain
        .multiply_position(wallet, &KAMINO_MAIN_MARKET)
        .await
        .ok()
        .flatten();
    let stable = chain
        .stable_yield_position(wallet, &KAMINO_MAIN_MARKET, &KAMINO_MAIN_USDC_RESERVE)
        .await
        .ok()
        .flatten();
    let hedge = chain.hedgedjlp_position(wallet).await.ok();

    let multiply_usd = multiply
        .as_ref()
        .map(|m| micro_to_usd(m.deposited_usd_micro.saturating_sub(m.borrowed_usd_micro)))
        .unwrap_or(0.0);
    // stable-yield: deposited cToken units; treat as USDC lamports at 6
    // decimals for display. This is approximate (cToken ↔ USDC needs the
    // reserve exchange rate); good enough for v0 dashboard.
    let stable_yield_usd = stable
        .as_ref()
        .map(|s| s.deposited_usdc_lamports as f64 / 1e6)
        .unwrap_or(0.0);
    let hedgedjlp_jlp_usd = hedge
        .as_ref()
        .map(|h| micro_to_usd(h.jlp_value_usd_micro))
        .unwrap_or(0.0);
    // rc16: sum the USDC collateral sitting inside every open Jupiter
    // Perps short — recovered via the same chain reader the dashboard
    // already uses to populate /positions.
    let hedgedjlp_collateral_usd = hedge
        .as_ref()
        .map(|h| {
            let sum: u128 = h
                .hedge_positions
                .iter()
                .map(|p| p.collateral_usd_micro as u128)
                .sum();
            micro_to_usd(sum.min(u64::MAX as u128) as u64)
        })
        .unwrap_or(0.0);
    let idle_usd = balances
        .as_ref()
        .map(|b| b.usdc_lamports as f64 / 1e6)
        .unwrap_or(0.0);

    ChainAumBreakdown {
        multiply_usd,
        stable_yield_usd,
        hedgedjlp_jlp_usd,
        hedgedjlp_collateral_usd,
        idle_usd,
    }
}

async fn aum(State(state): State<AppState>) -> impl IntoResponse {
    let wallet = state.wallet_pubkey;
    // rc24: single source of truth — both /aum and the aum_sampler call
    // `read_chain_aum_breakdown` so /pnl computed from snapshots matches
    // /aum's live read by construction.
    let breakdown = read_chain_aum_breakdown(&state.chain, &wallet).await;
    let multiply_usd = breakdown.multiply_usd;
    let stable_usd = breakdown.stable_yield_usd;
    let hedge_usd = breakdown.hedgedjlp_jlp_usd;
    let hedge_collateral_usd = breakdown.hedgedjlp_collateral_usd;
    let idle = breakdown.idle_usd;
    let total = breakdown.total_usd();

    // Combined APR: deployed-USD-weighted average of per-strategy APRs.
    // Idle capital is intentionally excluded (it earns 0% and would
    // otherwise dilute the operational-yield headline).
    let stable_apr = current_apr_bps_for("stable_yield", &state).await;
    let multiply_apr = current_apr_bps_for("multiply", &state).await;
    let hedge_apr = current_apr_bps_for("hedgedjlp", &state).await;
    let combined_apr_bps = weighted_combined_apr_bps(&[
        (stable_usd, stable_apr),
        (multiply_usd, multiply_apr),
        (hedge_usd, hedge_apr),
    ]);
    let deployed_total = stable_usd + multiply_usd + hedge_usd;
    let combined_annualised_usd = deployed_total * (combined_apr_bps as f64) / 10_000.0;

    Json(AumOut {
        total_usdc: total,
        per_strategy: PerStrategy {
            multiply: multiply_usd,
            stable_yield: stable_usd,
            hedgedjlp_jlp_value_usd: hedge_usd,
            hedgedjlp_collateral_usd: hedge_collateral_usd,
            idle_usdc: idle,
        },
        combined_apr_bps,
        combined_annualised_usd,
    })
}

#[derive(Debug, Deserialize)]
struct PnlQuery {
    window: Option<String>,
}

#[derive(Serialize)]
struct PnlOut {
    window: String,
    start_aum_usdc: f64,
    end_aum_usdc: f64,
    delta_usdc: f64,
    percent_bps: i32,
    elapsed_secs: u64,
    /// Per-daemon APY averaged — correct even when daemons have different
    /// elapsed times (e.g. one was restarted mid-soak). Frontends should
    /// prefer this over recomputing from delta/elapsed.
    annualised_apy_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

async fn pnl(State(state): State<AppState>, Query(q): Query<PnlQuery>) -> impl IntoResponse {
    let window = q.window.unwrap_or_else(|| "24h".to_string());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = match window.as_str() {
        "1h" => now - 3_600,
        "24h" => now - 86_400,
        "7d" => now - 7 * 86_400,
        "all" => 0,
        _ => now - 86_400,
    };

    // rc24 rewrite: read chain-state snapshots written by the
    // aum_sampler. /aum and /pnl now share the same source of truth —
    // a per-daemon telemetry desync (the rc24 incident class) can no
    // longer make /pnl disagree with /aum.
    let rows = match state.store.chain_aum_snapshots_since(cutoff).await {
        Ok(r) => r,
        Err(e) => {
            warn!(?e, "pnl: chain_aum_snapshots_since failed");
            return Json(PnlOut {
                window,
                start_aum_usdc: 0.0,
                end_aum_usdc: 0.0,
                delta_usdc: 0.0,
                percent_bps: 0,
                elapsed_secs: 0,
                annualised_apy_pct: 0.0,
                note: Some(format!("snapshot read failed: {e}")),
            })
            .into_response();
        }
    };

    if rows.is_empty() {
        return Json(PnlOut {
            window,
            start_aum_usdc: 0.0,
            end_aum_usdc: 0.0,
            delta_usdc: 0.0,
            percent_bps: 0,
            elapsed_secs: 0,
            annualised_apy_pct: 0.0,
            note: Some(
                "no chain_aum_snapshots in window — first snapshot fires 5s after dashboard boot"
                    .to_string(),
            ),
        })
        .into_response();
    }

    // Window bracket: oldest snapshot inside the cutoff vs newest.
    // `chain_aum_snapshots_since` returns oldest-first, so first()
    // and last() are correct without scanning past sentinel rows
    // (chain_aum_snapshots refuses to insert total_usd==0 — there ARE
    // no sentinels here, only real RPC reads).
    let first = rows.first().expect("rows non-empty checked above");
    let last = rows.last().expect("rows non-empty checked above");
    let start_sum = first.total_usd;
    let end_sum = last.total_usd;
    let elapsed_secs = if last.ts_unix > first.ts_unix {
        (last.ts_unix - first.ts_unix) as u64
    } else {
        // Single snapshot in window — no delta to compute.
        return Json(PnlOut {
            window,
            start_aum_usdc: start_sum,
            end_aum_usdc: end_sum,
            delta_usdc: 0.0,
            percent_bps: 0,
            elapsed_secs: 0,
            annualised_apy_pct: 0.0,
            note: Some(
                "only one snapshot in window — wait for another tick to see delta".to_string(),
            ),
        })
        .into_response();
    };

    let delta = end_sum - start_sum;
    let percent_bps = if start_sum.abs() > f64::EPSILON {
        ((delta / start_sum) * 10_000.0) as i32
    } else {
        0
    };
    const SECS_PER_YEAR: f64 = 365.0 * 24.0 * 3600.0;
    let annualised_apy_pct = if start_sum > 0.0 && elapsed_secs > 0 {
        (delta / start_sum) * (SECS_PER_YEAR / elapsed_secs as f64) * 100.0
    } else {
        0.0
    };

    Json(PnlOut {
        window,
        start_aum_usdc: start_sum,
        end_aum_usdc: end_sum,
        delta_usdc: delta,
        percent_bps,
        elapsed_secs,
        annualised_apy_pct,
        note: None,
    })
    .into_response()
}

// ── Paper trading ────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StrategyOut {
    id: &'static str,
    name: &'static str,
    tagline: &'static str,
    description: &'static str,
    principal_usdc: f64,
    net_apr_bps: u32,
    elapsed_secs: u64,
    earned_usdc: f64,
    total_aum_usdc: f64,
}

#[derive(Serialize)]
struct PortfolioOut {
    total_principal_usdc: f64,
    total_earned_usdc: f64,
    elapsed_secs: u64,
    annualised_apy_pct: f64,
}

#[derive(Serialize)]
struct PaperOut {
    strategies: Vec<StrategyOut>,
    portfolio: PortfolioOut,
}

struct StrategyMeta {
    daemon: &'static str,
    id: &'static str,
    name: &'static str,
    tagline: &'static str,
    description: &'static str,
    apr_field: &'static str,
}

const STRATEGIES: &[StrategyMeta] = &[
    StrategyMeta {
        daemon:      "stable_yield",
        id:          "stable_yield",
        name:        "Stable Yield",
        tagline:     "Kamino USDC supply — no leverage, no price exposure",
        description: "Deposits USDC into Kamino's main lending market and earns the live supply APR. Capital sits on-chain, fully liquid, zero directional risk. The floor of the portfolio — always positive carry.",
        apr_field:   "supply_apr_bps",
    },
    StrategyMeta {
        daemon:      "multiply",
        id:          "multiply",
        name:        "Multiply",
        tagline:     "2.5× leveraged jitoSOL via Kamino",
        description: "Deposits jitoSOL as collateral, borrows USDC at 60% LTV, and loops back into jitoSOL — amplifying native Solana staking yield plus Jito MEV tip rewards at 2.5× leverage. An autonomous agent monitors LTV and rebalances if it drifts.",
        apr_field:   "multiply_net_apr_bps",
    },
    StrategyMeta {
        daemon:      "hedgedjlp",
        id:          "hedgedjlp",
        name:        "Hedged JLP",
        tagline:     "Jupiter LP fees captured delta-neutral",
        description: "Buys JLP (Jupiter Liquidity Provider token) to earn trading-fee yield from Jupiter's perpetuals DEX, then opens a compensating short on Jupiter Perps to cancel all directional exposure. Net return is fee APY minus hedge borrow cost — effectively market-neutral yield.",
        apr_field:   "hedgedjlp_net_apr_bps",
    },
];

async fn paper_trading(State(state): State<AppState>) -> impl IntoResponse {
    let mut strategies: Vec<StrategyOut> = Vec::new();
    let mut total_principal = 0.0f64;
    let mut total_earned = 0.0f64;
    let mut max_elapsed = 0u64;

    for s in STRATEGIES {
        // recent_pnl_for with limit=1 returns the single newest snapshot.
        let row = state
            .store
            .recent_pnl_for(s.daemon, 1)
            .await
            .ok()
            .and_then(|mut rows| rows.pop());

        let (principal, elapsed, earned, aum, apr_bps) = match row {
            None => (50_000.0, 0, 0.0, 50_000.0, 0u32),
            Some((_, ref json)) => {
                let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
                let g = |key: &str| v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0);
                let principal = if g("paper_principal_usdc") > 0.0 {
                    g("paper_principal_usdc")
                } else {
                    50_000.0
                };
                let elapsed = v
                    .get("paper_elapsed_secs")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let earned = g("paper_earned_usdc");
                let aum = if g("total_aum_usdc") > 0.0 {
                    g("total_aum_usdc")
                } else {
                    principal
                };
                let apr_bps = v.get(s.apr_field).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                (principal, elapsed, earned, aum, apr_bps)
            }
        };

        total_principal += principal;
        total_earned += earned;
        if elapsed > max_elapsed {
            max_elapsed = elapsed;
        }

        strategies.push(StrategyOut {
            id: s.id,
            name: s.name,
            tagline: s.tagline,
            description: s.description,
            principal_usdc: principal,
            net_apr_bps: apr_bps,
            elapsed_secs: elapsed,
            earned_usdc: earned,
            total_aum_usdc: aum,
        });
    }

    const SECS_PER_YEAR: f64 = 365.0 * 24.0 * 3600.0;
    // Annualise per strategy then average. Using a single max_elapsed for all
    // strategies is wrong when daemons have different elapsed times (e.g. one
    // was restarted) — it deflates the portfolio number by penalising the
    // shorter-running strategy.
    let per_strategy_apys: Vec<f64> = strategies
        .iter()
        .filter(|s| s.elapsed_secs > 0 && s.principal_usdc > 0.0)
        .map(|s| {
            (s.earned_usdc / s.principal_usdc) * (SECS_PER_YEAR / s.elapsed_secs as f64) * 100.0
        })
        .collect();
    let annualised_apy = if per_strategy_apys.is_empty() {
        0.0
    } else {
        per_strategy_apys.iter().sum::<f64>() / per_strategy_apys.len() as f64
    };

    Json(PaperOut {
        strategies,
        portfolio: PortfolioOut {
            total_principal_usdc: total_principal,
            total_earned_usdc: total_earned,
            elapsed_secs: max_elapsed,
            annualised_apy_pct: annualised_apy,
        },
    })
}

/// Bug 3 filter: returns `true` for telemetry rows that represent paper
/// (simulated) positions rather than real on-chain capital.
///
/// Pull the deployed USD value out of a daemon's pnl JSONL row, derived
/// strictly from real on-chain position fields. Each daemon emits its
/// own shape (multiply: `net_equity_uusdc`; stable-yield:
/// `deposited_usdc_lamports`; hedgedjlp: `jlp_value_usd_micro`).
///
/// `total_aum_usdc` is deliberately ignored — every daemon emits that
/// as `paper_principal_usdc + paper_earned_usdc` regardless of mode, so
/// in live mode it's a meaningless synthetic baseline ($1k per daemon
/// by default). Rows that carry no non-zero real-position field return
/// `None` and are excluded from any AUM aggregation.
///
/// Field semantics:
/// - `*_uusdc` / `*_usdc_lamports` / `*_usd_micro` — integers in
///   micro-USD scale (1e-6). Divide by 1e6 to get USD.
/// - Anything else — not used. We never trust floating-point
///   `total_aum_usdc` / `paper_*` fields here.
/// rc24: this helper used to drive `/pnl` by parsing per-daemon
/// telemetry rows. The rewrite reads from chain-state snapshots
/// instead (see [`read_chain_aum_breakdown`]), so this function is
/// dead in production. Kept (with `#[allow(dead_code)]` for the lint)
/// for the unit tests that pin the daemon-telemetry JSON shape — those
/// shapes are still emitted by the daemons and any future helper that
/// wants to consume them as a *secondary* signal can reuse this.
#[allow(dead_code)]
fn pnl_row_to_usd(json: &str) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    for key in [
        "net_equity_uusdc",      // multiply: deposited − borrowed
        "deposited_usdc_lamports", // stable-yield: Kamino USDC supply
        "jlp_value_usd_micro",   // hedgedjlp: mark-to-market JLP value
    ] {
        if let Some(n) = v.get(key).and_then(|x| x.as_u64()) {
            if n > 0 {
                return Some(n as f64 / 1e6);
            }
        }
    }
    None
}

#[derive(Serialize)]
struct PositionsOut {
    multiply: Option<serde_json::Value>,
    stable_yield: Option<serde_json::Value>,
    hedgedjlp: Option<serde_json::Value>,
}

async fn positions(State(state): State<AppState>) -> impl IntoResponse {
    let wallet = state.wallet_pubkey;
    let multiply = state
        .chain
        .multiply_position(&wallet, &KAMINO_MAIN_MARKET)
        .await
        .ok()
        .flatten();
    let stable = state
        .chain
        .stable_yield_position(&wallet, &KAMINO_MAIN_MARKET, &KAMINO_MAIN_USDC_RESERVE)
        .await
        .ok()
        .flatten();
    let hedge = state.chain.hedgedjlp_position(&wallet).await.ok();

    Json(PositionsOut {
        multiply: multiply.and_then(|m| {
            serde_json::to_value(serde_json::json!({
                "obligation_pubkey": m.obligation_pubkey.to_string(),
                "ltv_bps": m.ltv_bps,
                "deposited_usd": micro_to_usd(m.deposited_usd_micro),
                "borrowed_usd": micro_to_usd(m.borrowed_usd_micro),
            }))
            .ok()
        }),
        stable_yield: stable.and_then(|s| {
            serde_json::to_value(serde_json::json!({
                "reserve_pubkey": s.reserve_pubkey.to_string(),
                "deposited_usdc": s.deposited_usdc_lamports as f64 / 1e6,
            }))
            .ok()
        }),
        hedgedjlp: hedge.and_then(|h| {
            let hedge_positions: Vec<serde_json::Value> = h
                .hedge_positions
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "asset": p.asset,
                        "size_usd": micro_to_usd(p.size_usd_micro),
                        "collateral_usd": micro_to_usd(p.collateral_usd_micro),
                        "side": p.side,
                        "position_pubkey": p.position_pubkey,
                    })
                })
                .collect();
            serde_json::to_value(serde_json::json!({
                "jlp_balance_lamports": h.jlp_balance_lamports,
                "jlp_value_usd": micro_to_usd(h.jlp_value_usd_micro),
                "hedge_positions": hedge_positions,
            }))
            .ok()
        }),
    })
}

#[derive(Serialize)]
struct DaemonOut {
    role: String,
    last_heartbeat_ms_ago: Option<i64>,
    status: &'static str,
}

async fn daemons(State(state): State<AppState>) -> impl IntoResponse {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let map: std::collections::HashMap<String, i64> = state
        .store
        .last_beacon_ts_by_role()
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    let out: Vec<DaemonOut> = DAEMON_ROLES
        .iter()
        .map(|role| {
            let last_ts = map.get(*role).copied();
            let (ago, status) = match last_ts {
                None => (None, "unknown"),
                Some(ts) => {
                    let ago = (now_ms - ts).max(0);
                    let status = if ago < STATUS_GREEN_MS {
                        "green"
                    } else if ago < STATUS_YELLOW_MS {
                        "yellow"
                    } else {
                        "red"
                    };
                    (Some(ago), status)
                }
            };
            DaemonOut {
                role: role.to_string(),
                last_heartbeat_ms_ago: ago,
                status,
            }
        })
        .collect();
    Json(out)
}

// ── /strategies ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct StrategyCardOut {
    id: &'static str,
    name: &'static str,
    tagline: &'static str,
    description: &'static str,
    /// `"live"` if `deployed_usdc > 0`, else `"idle"`.
    status: &'static str,
    /// For stable_yield + multiply: live net equity (deposit - borrow).
    /// For hedgedjlp: mark-to-market JLP value only. The corresponding
    /// hedge collateral is reported separately in `hedge_collateral_usdc`
    /// so the APR math (which the daemon prices against JLP value)
    /// remains consistent. Sum the two on the frontend to display the
    /// operator's total committed capital in the strategy.
    deployed_usdc: f64,
    /// rc16: USDC sitting as collateral inside Jupiter Perps short
    /// positions. `Some` only for hedgedjlp; `None` for strategies that
    /// don't have a separate collateral surface. Frontend renders this
    /// as a secondary "+ $X collateral" line beneath the primary
    /// deployed figure.
    #[serde(skip_serializing_if = "Option::is_none")]
    hedge_collateral_usdc: Option<f64>,
    /// Live APR in basis points. For stable-yield this is the Kamino
    /// supply rate from `/rates`. For multiply and hedgedjlp it is read
    /// from the most recent pnl_snapshot row (the daemon's own APR
    /// estimate). 0 when unknown — the frontend renders "—".
    current_apr_bps: u32,
    /// Most recent confirmed on-chain signature emitted by this daemon,
    /// or `None` if it has not yet broadcast anything. The frontend uses
    /// this to render a "View on-chain →" Solscan link.
    last_sig: Option<String>,
    // TODO: surface `earned_usdc` once we have a reliable starting cost
    // basis lookup. Per the v0.1.9 spec we omit it for now rather than
    // ship a number we can't defend (the pnl_snapshot earned field
    // currently mixes paper and real components).
}

#[derive(Serialize)]
struct StrategiesOut {
    strategies: Vec<StrategyCardOut>,
}

async fn strategies(State(state): State<AppState>) -> impl IntoResponse {
    let wallet = state.wallet_pubkey;

    // On-chain deployed USDC per strategy — same shape as /aum.
    let multiply = state
        .chain
        .multiply_position(&wallet, &KAMINO_MAIN_MARKET)
        .await
        .ok()
        .flatten();
    let stable = state
        .chain
        .stable_yield_position(&wallet, &KAMINO_MAIN_MARKET, &KAMINO_MAIN_USDC_RESERVE)
        .await
        .ok()
        .flatten();
    let hedge = state.chain.hedgedjlp_position(&wallet).await.ok();

    let multiply_usd = multiply
        .as_ref()
        .map(|m| micro_to_usd(m.deposited_usd_micro.saturating_sub(m.borrowed_usd_micro)))
        .unwrap_or(0.0);
    let stable_usd = stable
        .as_ref()
        .map(|s| s.deposited_usdc_lamports as f64 / 1e6)
        .unwrap_or(0.0);
    let hedge_usd = hedge
        .as_ref()
        .map(|h| micro_to_usd(h.jlp_value_usd_micro))
        .unwrap_or(0.0);
    // rc16: same sum as /aum so the two endpoints agree to the cent.
    let hedge_collateral_usd: f64 = hedge
        .as_ref()
        .map(|h| {
            let sum: u128 = h
                .hedge_positions
                .iter()
                .map(|p| p.collateral_usd_micro as u128)
                .sum();
            micro_to_usd(sum.min(u64::MAX as u128) as u64)
        })
        .unwrap_or(0.0);

    let rates = state.chain.rate_snapshot().await;

    let mut out: Vec<StrategyCardOut> = Vec::with_capacity(STRATEGIES.len());
    for s in STRATEGIES {
        let deployed = match s.id {
            "stable_yield" => stable_usd,
            "multiply" => multiply_usd,
            "hedgedjlp" => hedge_usd,
            _ => 0.0,
        };
        // hedgedjlp is the only strategy where collateral lives outside
        // the strategy's primary value surface. Other strategies fold
        // their collateral into `deployed_usdc` natively (e.g. multiply's
        // jitoSOL collateral is reflected in `deposited_usd_micro`).
        let hedge_collateral_usdc = match s.id {
            "hedgedjlp" if hedge_collateral_usd > 0.0 => Some(hedge_collateral_usd),
            _ => None,
        };
        // Strategy is "live" if either the primary deployment OR the
        // collateral is non-zero. Catches the edge case where the JLP
        // got swapped out mid-unwind but the short collateral is still
        // locked — still "live" for operator decision purposes.
        let status = if deployed > 0.0 || hedge_collateral_usdc.is_some() {
            "live"
        } else {
            "idle"
        };

        // APR sourcing:
        //   stable-yield → Kamino USDC supply rate from /rates.
        //   multiply / hedgedjlp → daemon's own apr field on its newest
        //                          pnl_snapshot row, when one exists.
        let current_apr_bps: u32 = if s.id == "stable_yield" {
            rates.kamino_usdc_supply_bps
        } else {
            let newest = state
                .store
                .recent_pnl_for(s.daemon, 1)
                .await
                .ok()
                .and_then(|mut rows| rows.pop());
            match newest {
                None => 0,
                Some((_, json)) => {
                    let v: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
                    v.get(s.apr_field).and_then(|x| x.as_u64()).unwrap_or(0) as u32
                }
            }
        };

        let last_sig = state.store.last_sig_for_role(s.daemon).await.ok().flatten();

        out.push(StrategyCardOut {
            id: s.id,
            name: s.name,
            tagline: s.tagline,
            description: s.description,
            status,
            deployed_usdc: deployed,
            hedge_collateral_usdc,
            current_apr_bps,
            last_sig,
        });
    }

    Json(StrategiesOut { strategies: out })
}

fn micro_to_usd(micro: u64) -> f64 {
    micro as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pnl_row_to_usd_extracts_multiply_net_equity() {
        // Live multiply row: deposited - borrowed = net equity in u-USDC.
        let row = r#"{
          "timestamp_unix": 1779091295,
          "deposited_uusdc": 8773636,
          "borrowed_uusdc": 2412080,
          "net_equity_uusdc": 6361556,
          "paper_principal_usdc": 1000.0,
          "total_aum_usdc": 1000.099
        }"#;
        // Must use net_equity_uusdc, never total_aum_usdc.
        assert_eq!(pnl_row_to_usd(row), Some(6.361556));
    }

    #[test]
    fn pnl_row_to_usd_extracts_stable_yield_deposited() {
        let row = r#"{
          "ts": 1779091274,
          "deposited_usdc_lamports": 46562924,
          "paper_principal_usdc": 1000.0,
          "total_aum_usdc": 1000.425
        }"#;
        assert_eq!(pnl_row_to_usd(row), Some(46.562924));
    }

    #[test]
    fn pnl_row_to_usd_extracts_hedgedjlp_jlp_value() {
        let row = r#"{
          "ts": 1779091274,
          "jlp_value_usd_micro": 174512042,
          "jlp_lamports": 45165980,
          "paper_principal_usdc": 1000.0,
          "total_aum_usdc": 1000.0
        }"#;
        assert_eq!(pnl_row_to_usd(row), Some(174.512042));
    }

    #[test]
    fn pnl_row_to_usd_returns_none_for_synthetic_only_rows() {
        // Paper-mode multiply row: real fields are all zero, only paper
        // synthetics are populated. Must contribute nothing to AUM.
        let paper_50k = r#"{
          "timestamp_unix": 1778777669,
          "deposited_uusdc": 0,
          "borrowed_uusdc": 0,
          "net_equity_uusdc": 0,
          "paper_principal_usdc": 50000.0,
          "total_aum_usdc": 50000.005
        }"#;
        assert_eq!(pnl_row_to_usd(paper_50k), None);

        // Live-mode synthetic baseline ($1k paper_principal): real
        // fields zero (daemon hadn't deployed yet). Still None.
        let paper_1k = r#"{
          "ts": 1778878488,
          "jlp_lamports": 0,
          "jlp_value_usd_micro": 0,
          "paper_principal_usdc": 1000.0,
          "total_aum_usdc": 1003.07
        }"#;
        assert_eq!(pnl_row_to_usd(paper_1k), None);
    }

    #[test]
    fn pnl_row_to_usd_handles_malformed_input() {
        assert_eq!(pnl_row_to_usd("{not json"), None);
        assert_eq!(pnl_row_to_usd("{}"), None);
        // Field present but wrong type (string instead of int) → None.
        assert_eq!(
            pnl_row_to_usd(r#"{"net_equity_uusdc":"6361556"}"#),
            None
        );
    }

    #[test]
    fn strategies_metadata_covers_all_three_yield_daemons() {
        // /strategies returns exactly the three institutional cards.
        let ids: Vec<&str> = STRATEGIES.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["stable_yield", "multiply", "hedgedjlp"]);
        // None of the taglines or descriptions are empty — the dashboard
        // relies on these as institutional pitch copy.
        for s in STRATEGIES {
            assert!(!s.tagline.is_empty(), "{} tagline missing", s.id);
            assert!(!s.description.is_empty(), "{} description missing", s.id);
        }
    }

    #[test]
    fn apr_compute_stats_min_max_mean_p50() {
        let samples: Vec<AprSample> = [900, 950, 940, 920, 1019, 880, 935]
            .iter()
            .enumerate()
            .map(|(i, bps)| AprSample {
                ts_ms: 1_000_000 + i as i64 * 60_000,
                apr_bps: *bps,
            })
            .collect();
        let stats = compute_stats(&samples);
        assert_eq!(stats.min_bps, 880);
        assert_eq!(stats.max_bps, 1019);
        // sum=6544, n=7, mean=934 (floor div).
        assert_eq!(stats.mean_bps, 934);
        // sorted: 880, 900, 920, 935, 940, 950, 1019 → p50 = vals[3] = 935.
        assert_eq!(stats.p50_bps, 935);
        assert_eq!(stats.samples_count, 7);
    }

    #[test]
    fn apr_compute_stats_handles_empty() {
        let stats = compute_stats(&[]);
        assert_eq!(stats.samples_count, 0);
        assert_eq!(stats.min_bps, 0);
        assert_eq!(stats.max_bps, 0);
        assert_eq!(stats.mean_bps, 0);
        assert_eq!(stats.p50_bps, 0);
    }

    #[test]
    fn weighted_combined_apr_matches_fleet_snapshot() {
        // Reproduces v0.2.8 launch fleet state:
        //   stable_yield $55.16 × 982 bps
        //   multiply     $8.85  × 496 bps
        //   hedgedjlp    $179.93 × 1608 bps
        // Σ deployed = $243.94, Σ weighted = 347,884.74 → 1426 bps.
        let bps = weighted_combined_apr_bps(&[(55.16, 982), (8.85, 496), (179.93, 1608)]);
        assert_eq!(bps, 1426);
    }

    #[test]
    fn weighted_combined_apr_returns_zero_when_no_capital_deployed() {
        assert_eq!(weighted_combined_apr_bps(&[]), 0);
        assert_eq!(weighted_combined_apr_bps(&[(0.0, 982), (0.0, 1608)]), 0);
    }

    #[test]
    fn weighted_combined_apr_ignores_zero_usd_legs() {
        // Idle-excluded behaviour: a 0-USD leg contributes nothing.
        let bps = weighted_combined_apr_bps(&[(100.0, 1000), (0.0, 5000)]);
        assert_eq!(bps, 1000);
    }

    #[test]
    fn aum_per_strategy_includes_hedgedjlp_collateral_field() {
        // rc16 contract: /aum must serialize the new field even when the
        // value is zero (frontend distinguishes "no collateral" from
        // "field missing"). This pins the JSON shape against accidental
        // field removal or rename.
        let out = AumOut {
            total_usdc: 247.0,
            per_strategy: PerStrategy {
                multiply: 8.40,
                stable_yield: 55.21,
                hedgedjlp_jlp_value_usd: 119.66,
                hedgedjlp_collateral_usd: 63.41,
                idle_usdc: 1.0,
            },
            combined_apr_bps: 1000,
            combined_annualised_usd: 18.4,
        };
        let v = serde_json::to_value(&out).expect("AumOut serializable");
        let per = v.get("per_strategy").expect("per_strategy present");
        // Existing fields untouched
        assert_eq!(per.get("multiply"), Some(&serde_json::json!(8.40)));
        assert_eq!(per.get("stable_yield"), Some(&serde_json::json!(55.21)));
        assert_eq!(
            per.get("hedgedjlp_jlp_value_usd"),
            Some(&serde_json::json!(119.66))
        );
        assert_eq!(per.get("idle_usdc"), Some(&serde_json::json!(1.0)));
        // New rc16 field
        assert_eq!(
            per.get("hedgedjlp_collateral_usd"),
            Some(&serde_json::json!(63.41))
        );
        // Topline includes collateral
        assert_eq!(v.get("total_usdc"), Some(&serde_json::json!(247.0)));
    }

    #[test]
    fn strategy_card_omits_hedge_collateral_for_non_hedgedjlp() {
        // The frontend type `hedge_collateral_usdc?: number` expects the
        // field to be absent (not null) for stable_yield + multiply.
        // `#[serde(skip_serializing_if = "Option::is_none")]` handles this.
        let card = StrategyCardOut {
            id: "stable_yield",
            name: "Stable Yield",
            tagline: "x",
            description: "y",
            status: "live",
            deployed_usdc: 55.21,
            hedge_collateral_usdc: None,
            current_apr_bps: 542,
            last_sig: None,
        };
        let v = serde_json::to_value(&card).expect("StrategyCardOut serializable");
        let obj = v.as_object().expect("object");
        assert!(
            !obj.contains_key("hedge_collateral_usdc"),
            "field must be omitted, not null, for non-hedgedjlp strategies"
        );
    }

    #[test]
    fn strategy_card_emits_hedge_collateral_for_hedgedjlp() {
        let card = StrategyCardOut {
            id: "hedgedjlp",
            name: "Hedged JLP",
            tagline: "x",
            description: "y",
            status: "live",
            deployed_usdc: 119.66,
            hedge_collateral_usdc: Some(63.41),
            current_apr_bps: 1012,
            last_sig: None,
        };
        let v = serde_json::to_value(&card).expect("StrategyCardOut serializable");
        assert_eq!(
            v.get("hedge_collateral_usdc"),
            Some(&serde_json::json!(63.41))
        );
        assert_eq!(v.get("deployed_usdc"), Some(&serde_json::json!(119.66)));
    }

    #[test]
    fn hedge_collateral_sum_handles_empty_positions() {
        // Defense: a hedgedjlp position with no open shorts (just JLP)
        // must return zero collateral, not panic or saturate. This is
        // the steady state right before the first Assign.
        use crate::chain::jupiter_perps::PositionView;
        let view = PositionView {
            jlp_balance_lamports: 0,
            jlp_value_usd_micro: 0,
            hedge_positions: vec![],
        };
        let sum: u128 = view
            .hedge_positions
            .iter()
            .map(|p| p.collateral_usd_micro as u128)
            .sum();
        assert_eq!(sum, 0);
    }

    #[test]
    fn lifetime_constants_match_devlog() {
        // 2026-05-09T00:00:00Z = 1_778_284_800.
        //
        // rc21 audit M1: rc20 hardened the test with a one-sided
        // year-floor (`>= 2026-01-01`) which catches off-by-year-
        // backward but not a typo to 2029. Two-sided + LITEPAPER
        // cross-check below makes the test independent of the
        // constant rather than paraphrasing it.
        assert_eq!(LIVE_SINCE_UNIX, 1_778_284_800);

        // Lower bound: 2026-01-01T00:00:00Z = 1_767_225_600. Catches
        // off-by-year-backward (rc20 incident).
        const Y2026_UNIX: i64 = 1_767_225_600;
        assert!(
            LIVE_SINCE_UNIX >= Y2026_UNIX,
            "LIVE_SINCE_UNIX ({LIVE_SINCE_UNIX}) is before 2026-01-01 — \
             likely an off-by-one-year typo"
        );

        // Upper bound: must be in the past at test time. Catches the
        // mirror case (a typo to a future year). Computed at test run
        // rather than hardcoded so the assertion stays valid as time
        // moves on.
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_secs() as i64;
        assert!(
            LIVE_SINCE_UNIX <= now_unix,
            "LIVE_SINCE_UNIX ({LIVE_SINCE_UNIX}) is in the future \
             (now={now_unix}) — the live deployment can't predate now"
        );

        // Cross-check the LITEPAPER. The marketing document and the
        // code's `live since` must agree to the day, otherwise the
        // hero banner contradicts the institutional pitch. Including
        // the LITEPAPER at compile time makes it the single source of
        // truth for the genesis date — flip one without the other and
        // CI fails.
        const LITEPAPER: &str = include_str!("../../../../LITEPAPER.md");
        assert!(
            LITEPAPER.contains("Live mainnet operation since 2026-05-09"),
            "LITEPAPER.md no longer claims 'Live mainnet operation since \
             2026-05-09'. Either the live-since date changed (in which case \
             update LIVE_SINCE_UNIX to match) or the LITEPAPER copy drifted \
             (in which case restore the canonical sentence)."
        );

        // Sanity: incidents_resolved is positive and within a sane
        // range. The test pins the floor; the ceiling exists to catch
        // an accidental "= 1800" typo.
        assert!(INCIDENTS_RESOLVED >= 18);
        assert!(INCIDENTS_RESOLVED < 1000);
    }

    #[test]
    fn lifetime_output_shape_serializes() {
        // rc21 audit L1: seed with the real 2026-05-09 timestamp, not
        // the rc19 typo. Test data should never encode a value we
        // declared incorrect — future readers shouldn't have to guess
        // whether the literal is meaningful.
        let out = LifetimeOut {
            live_since_unix: LIVE_SINCE_UNIX,
            now_unix: LIVE_SINCE_UNIX + 86_400 * 11,
            uptime_secs: 86_400 * 11,
            incidents_resolved: 18,
        };
        let v = serde_json::to_value(&out).expect("LifetimeOut serializable");
        // Frontend depends on these field names — pin them.
        assert_eq!(
            v.get("live_since_unix"),
            Some(&serde_json::json!(LIVE_SINCE_UNIX))
        );
        assert_eq!(v.get("incidents_resolved"), Some(&serde_json::json!(18)));
        assert!(v.get("uptime_secs").is_some());
        assert!(v.get("now_unix").is_some());
    }

    #[test]
    fn status_thresholds_are_ordered_and_in_seconds_not_micros() {
        // rc21 audit M2: pre-rc21 these constants had NO test. A typo
        // changing 30_000 (ms) to 30_000_000 (µs by accident) would
        // make every daemon yellow at boot and break the dashboard's
        // health pill. Catch the ordering invariant + the millisecond
        // unit range here.
        assert!(
            STATUS_GREEN_MS < STATUS_YELLOW_MS,
            "STATUS_GREEN_MS ({STATUS_GREEN_MS}) must be < STATUS_YELLOW_MS \
             ({STATUS_YELLOW_MS}) — otherwise daemons can never be 'green'"
        );
        assert!(STATUS_GREEN_MS > 0);
        // Sanity range: a green daemon must heartbeat within 60s; the
        // yellow band must close within 10 minutes. Both bounds are
        // operationally chosen — looser would mask real outages,
        // tighter would noise on transient RPC latency.
        assert!(
            (10_000..=60_000).contains(&STATUS_GREEN_MS),
            "STATUS_GREEN_MS ({STATUS_GREEN_MS}) outside 10s-60s window — \
             likely a unit typo"
        );
        assert!(
            (60_000..=600_000).contains(&STATUS_YELLOW_MS),
            "STATUS_YELLOW_MS ({STATUS_YELLOW_MS}) outside 60s-10min window — \
             likely a unit typo"
        );
    }

    #[test]
    fn hedge_collateral_sum_aggregates_three_shorts() {
        use crate::chain::jupiter_perps::{HedgePosition, PositionView};
        let view = PositionView {
            jlp_balance_lamports: 45_000_000,
            jlp_value_usd_micro: 119_660_000,
            hedge_positions: vec![
                HedgePosition {
                    asset: "SOL".into(),
                    size_usd_micro: 138_564_160,
                    collateral_usd_micro: 27_700_000,
                    side: "Short".into(),
                    position_pubkey: "x".into(),
                },
                HedgePosition {
                    asset: "ETH".into(),
                    size_usd_micro: 33_983_007,
                    collateral_usd_micro: 6_800_000,
                    side: "Short".into(),
                    position_pubkey: "y".into(),
                },
                HedgePosition {
                    asset: "BTC".into(),
                    size_usd_micro: 40_404_837,
                    collateral_usd_micro: 8_080_000,
                    side: "Short".into(),
                    position_pubkey: "z".into(),
                },
            ],
        };
        let sum: u128 = view
            .hedge_positions
            .iter()
            .map(|p| p.collateral_usd_micro as u128)
            .sum();
        assert_eq!(sum, 27_700_000 + 6_800_000 + 8_080_000);
        let usd = micro_to_usd(sum.min(u64::MAX as u128) as u64);
        assert!((usd - 42.58).abs() < 1e-6, "expected 42.58, got {usd}");
    }
}
