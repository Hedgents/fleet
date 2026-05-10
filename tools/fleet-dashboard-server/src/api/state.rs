//! `GET /aum`, `GET /pnl`, `GET /positions`, `GET /daemons`.

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

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

    Json(AumOut {
        total_usdc: total,
        per_strategy: PerStrategy {
            multiply: multiply_usd,
            stable_yield: stable_usd,
            hedgedjlp_jlp_value_usd: hedge_usd,
            idle_usdc: idle,
        },
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
            // recent_pnl_for reverses internally → oldest-first
            let in_window: Vec<_> = rows
                .iter()
                .filter(|(ts, _)| *ts >= cutoff)
                .collect();
            if in_window.is_empty() {
                continue;
            }
            let start_ts = in_window.first().map(|(ts, _)| *ts).unwrap_or(0);
            let end_ts   = in_window.last().map(|(ts, _)| *ts).unwrap_or(0);
            let start_val = in_window.first().and_then(|(_, j)| pnl_row_to_usd(j));
            let end_val   = in_window.last().and_then(|(_, j)| pnl_row_to_usd(j));
            if let (Some(s), Some(e)) = (start_val, end_val) {
                start_sum += s;
                end_sum += e;
                found += 1;
                min_ts = min_ts.min(start_ts);
                max_ts = max_ts.max(end_ts);
                // Per-daemon elapsed avoids the restart artifact: a restarted
                // daemon's window is shorter but its APY is still valid.
                let daemon_elapsed = if end_ts > start_ts { (end_ts - start_ts) as f64 } else { 1.0 };
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

    let elapsed_secs = if max_ts > min_ts { (max_ts - min_ts) as u64 } else { 1 };
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
    daemon:      &'static str,
    id:          &'static str,
    name:        &'static str,
    tagline:     &'static str,
    description: &'static str,
    apr_field:   &'static str,
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
    let mut total_earned    = 0.0f64;
    let mut max_elapsed     = 0u64;

    for s in STRATEGIES {
        // recent_pnl_for with limit=1 returns the single newest snapshot.
        let row = state.store.recent_pnl_for(s.daemon, 1).await
            .ok()
            .and_then(|mut rows| rows.pop());

        let (principal, elapsed, earned, aum, apr_bps) = match row {
            None => (50_000.0, 0, 0.0, 50_000.0, 0u32),
            Some((_, ref json)) => {
                let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
                let g = |key: &str| v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0);
                let principal = if g("paper_principal_usdc") > 0.0 { g("paper_principal_usdc") } else { 50_000.0 };
                let elapsed   = v.get("paper_elapsed_secs").and_then(|x| x.as_u64()).unwrap_or(0);
                let earned    = g("paper_earned_usdc");
                let aum       = if g("total_aum_usdc") > 0.0 { g("total_aum_usdc") } else { principal };
                let apr_bps   = v.get(s.apr_field).and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                (principal, elapsed, earned, aum, apr_bps)
            }
        };

        total_principal += principal;
        total_earned    += earned;
        if elapsed > max_elapsed { max_elapsed = elapsed; }

        strategies.push(StrategyOut {
            id:          s.id,
            name:        s.name,
            tagline:     s.tagline,
            description: s.description,
            principal_usdc:  principal,
            net_apr_bps:     apr_bps,
            elapsed_secs:    elapsed,
            earned_usdc:     earned,
            total_aum_usdc:  aum,
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
            total_earned_usdc:    total_earned,
            elapsed_secs:         max_elapsed,
            annualised_apy_pct:   annualised_apy,
        },
    })
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
            serde_json::to_value(serde_json::json!({
                "jlp_balance_lamports": h.jlp_balance_lamports,
                "jlp_value_usd": micro_to_usd(h.jlp_value_usd_micro),
                "hedge_positions": h.hedge_positions,
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

fn micro_to_usd(micro: u64) -> f64 {
    micro as f64 / 1_000_000.0
}
