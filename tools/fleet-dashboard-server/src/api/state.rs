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

use crate::api::AppState;
use zerox1_defi_protocols::constants::{KAMINO_MAIN_MARKET, KAMINO_MAIN_USDC_RESERVE};

const DAEMON_ROLES: &[&str] = &[
    "multiply",
    "stable_yield",
    "hedgedjlp",
    "riskwatcher",
    "researcher",
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
    hedgedjlp_jlp_value_usd: f64,
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

async fn aum(State(state): State<AppState>) -> impl IntoResponse {
    let wallet = state.wallet_pubkey;
    let balances = state.chain.wallet_balances(&wallet).await.ok();
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
    // stable-yield: deposited cToken units; treat as USDC lamports at 6
    // decimals for display. This is approximate (cToken ↔ USDC needs the
    // reserve exchange rate); good enough for v0 dashboard.
    let stable_usd = stable
        .as_ref()
        .map(|s| s.deposited_usdc_lamports as f64 / 1e6)
        .unwrap_or(0.0);
    let hedge_usd = hedge
        .as_ref()
        .map(|h| micro_to_usd(h.jlp_value_usd_micro))
        .unwrap_or(0.0);
    let idle = balances
        .as_ref()
        .map(|b| b.usdc_lamports as f64 / 1e6)
        .unwrap_or(0.0);
    let total = multiply_usd + stable_usd + hedge_usd + idle;

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
        "all" => 0,
        _ => now - 86_400,
    };

    // Per-daemon delta: avoids the "daemon started later" artifact where the
    // carry-forward time series shows a jump when a new daemon comes online.
    // For each yield daemon: start = oldest snapshot in window, end = newest.
    // Sum start_sum and end_sum independently; delta = end_sum - start_sum.
    const YIELD_DAEMONS: &[&str] = &["multiply", "stable_yield", "hedgedjlp"];
    const SECS_PER_YEAR: f64 = 365.0 * 24.0 * 3600.0;
    let mut start_sum = 0.0f64;
    let mut end_sum = 0.0f64;
    let mut found = 0usize;
    let mut min_ts = i64::MAX;
    let mut max_ts = i64::MIN;
    let mut per_daemon_apys: Vec<f64> = Vec::new();

    for d in YIELD_DAEMONS {
        if let Ok(rows) = state.store.recent_pnl_for(d, 5_000).await {
            // recent_pnl_for reverses internally → oldest-first.
            // Bug 3: multiply + hedgedjlp emit paper telemetry with
            // paper_principal_usdc=50_000 (driven by
            // --paper-principal-usdc-lamports=50000000000). The
            // 24h-window rollup averaged these synthetic $50k positions
            // into an apparent $100k AUM. Real positions today are all
            // <$1k, so any row reporting paper_principal_usdc ≥ $10k is
            // demonstrably synthetic and must be excluded from the AUM
            // delta. See `is_paper_row`.
            let in_window: Vec<_> = rows
                .iter()
                .filter(|(ts, _)| *ts >= cutoff)
                .filter(|(_, j)| !is_paper_row(j))
                .collect();
            if in_window.is_empty() {
                continue;
            }
            let start_ts = in_window.first().map(|(ts, _)| *ts).unwrap_or(0);
            let end_ts = in_window.last().map(|(ts, _)| *ts).unwrap_or(0);
            let start_val = in_window.first().and_then(|(_, j)| pnl_row_to_usd(j));
            let end_val = in_window.last().and_then(|(_, j)| pnl_row_to_usd(j));
            if let (Some(s), Some(e)) = (start_val, end_val) {
                start_sum += s;
                end_sum += e;
                found += 1;
                min_ts = min_ts.min(start_ts);
                max_ts = max_ts.max(end_ts);
                // Per-daemon elapsed avoids the restart artifact: a restarted
                // daemon's window is shorter but its APY is still valid.
                let daemon_elapsed = if end_ts > start_ts {
                    (end_ts - start_ts) as f64
                } else {
                    1.0
                };
                if s > 0.0 && daemon_elapsed > 60.0 {
                    per_daemon_apys.push((e - s) / s * (SECS_PER_YEAR / daemon_elapsed) * 100.0);
                }
            }
        }
    }

    if found == 0 {
        return Json(PnlOut {
            window,
            start_aum_usdc: 0.0,
            end_aum_usdc: 0.0,
            delta_usdc: 0.0,
            percent_bps: 0,
            elapsed_secs: 0,
            annualised_apy_pct: 0.0,
            note: Some("no pnl_snapshot history yet".to_string()),
        })
        .into_response();
    }

    let elapsed_secs = if max_ts > min_ts {
        (max_ts - min_ts) as u64
    } else {
        1
    };
    let delta = end_sum - start_sum;
    let percent_bps = if start_sum.abs() > f64::EPSILON {
        ((delta / start_sum) * 10_000.0) as i32
    } else {
        0
    };
    let annualised_apy_pct = if per_daemon_apys.is_empty() {
        0.0
    } else {
        per_daemon_apys.iter().sum::<f64>() / per_daemon_apys.len() as f64
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
/// We classify any row carrying `paper_principal_usdc >= PAPER_THRESHOLD`
/// as paper-only. Real fleet positions today are all under $1k; the paper
/// runners are configured with $50k principals. A $10k threshold gives a
/// 10× safety margin on both sides and remains valid until real positions
/// exceed $10k, at which point this filter should be revisited.
const PAPER_PRINCIPAL_THRESHOLD_USDC: f64 = 10_000.0;

pub(crate) fn is_paper_row(json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return false;
    };
    v.get("paper_principal_usdc")
        .and_then(|x| x.as_f64())
        .map(|p| p >= PAPER_PRINCIPAL_THRESHOLD_USDC)
        .unwrap_or(false)
}

/// Pull a USD-ish value out of a daemon's pnl JSONL row. Best-effort:
/// looks for common field names. Returns `None` if nothing matches.
fn pnl_row_to_usd(json: &str) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    // Try several common keys daemons emit.
    for key in [
        "total_aum_usdc",
        "aum_usdc",
        "deposited_usdc",
        "deposited_usd",
        "position_value_usd",
        "net_value_usd",
    ] {
        if let Some(n) = v.get(key).and_then(|x| x.as_f64()) {
            return Some(n);
        }
    }
    // Lamport-style fallback.
    for key in ["deposited_usdc_lamports", "aum_usdc_lamports"] {
        if let Some(n) = v.get(key).and_then(|x| x.as_u64()) {
            return Some(n as f64 / 1e6);
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
    deployed_usdc: f64,
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

    let rates = state.chain.rate_snapshot().await;

    let mut out: Vec<StrategyCardOut> = Vec::with_capacity(STRATEGIES.len());
    for s in STRATEGIES {
        let deployed = match s.id {
            "stable_yield" => stable_usd,
            "multiply" => multiply_usd,
            "hedgedjlp" => hedge_usd,
            _ => 0.0,
        };
        let status = if deployed > 0.0 { "live" } else { "idle" };

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
    fn is_paper_row_flags_synthetic_50k_principal() {
        // Multiply / hedgedjlp paper telemetry.
        let row = r#"{"ts":1,"paper_principal_usdc":50000.0,"total_aum_usdc":50001.2}"#;
        assert!(
            is_paper_row(row),
            "rows with paper_principal_usdc≥$10k must be filtered"
        );
    }

    #[test]
    fn is_paper_row_keeps_real_small_positions() {
        // Real stable-yield row with on-chain $54 deposit and no paper field.
        let real = r#"{"ts":1,"total_aum_usdc":54.23,"deposited_usdc":54.23}"#;
        assert!(
            !is_paper_row(real),
            "real on-chain row must not be filtered"
        );

        // Real row that also carries a small paper figure (daemon also
        // simulates for charts) — still under threshold.
        let real_with_small_paper =
            r#"{"ts":1,"paper_principal_usdc":50.0,"total_aum_usdc":54.23}"#;
        assert!(
            !is_paper_row(real_with_small_paper),
            "rows with paper_principal_usdc < $10k must not be filtered"
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
    fn is_paper_row_handles_malformed_json() {
        assert!(
            !is_paper_row("{not json"),
            "malformed JSON should not be filtered as paper"
        );
        assert!(
            !is_paper_row("{}"),
            "empty object should not be filtered as paper"
        );
    }
}
