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
        .route("/positions", get(positions))
        .route("/daemons", get(daemons))
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

    // Aggregate across all daemon pnl_snapshots: take the row closest to
    // (>=) `cutoff` per daemon as the "start", and the latest as "end".
    // For v0 we use the simpler global approach: pick the oldest row
    // in-window across all daemons as start, the newest as end.
    let snapshots = collect_aum_series(&state).await;
    if snapshots.is_empty() {
        return Json(PnlOut {
            window,
            start_aum_usdc: 0.0,
            end_aum_usdc: 0.0,
            delta_usdc: 0.0,
            percent_bps: 0,
            note: Some("no pnl_snapshot history yet".to_string()),
        })
        .into_response();
    }
    let in_window: Vec<_> = snapshots
        .iter()
        .filter(|(ts, _)| *ts >= cutoff)
        .copied()
        .collect();
    if in_window.len() < 2 {
        return Json(PnlOut {
            window,
            start_aum_usdc: 0.0,
            end_aum_usdc: 0.0,
            delta_usdc: 0.0,
            percent_bps: 0,
            note: Some("insufficient pnl history in window".to_string()),
        })
        .into_response();
    }
    let start = in_window.first().unwrap().1;
    let end = in_window.last().unwrap().1;
    let delta = end - start;
    let percent_bps = if start.abs() > f64::EPSILON {
        ((delta / start) * 10_000.0) as i32
    } else {
        0
    };
    Json(PnlOut {
        window,
        start_aum_usdc: start,
        end_aum_usdc: end,
        delta_usdc: delta,
        percent_bps,
        note: None,
    })
    .into_response()
}

/// Best-effort time series of total AUM derived from pnl_snapshots.
/// For v0 we sum the latest known per-daemon value at each timestamp;
/// since rows arrive at slightly different cadences we use a per-daemon
/// "latest seen" carry-forward. Simple, monotone-ish; OK for demo P&L.
async fn collect_aum_series(state: &AppState) -> Vec<(i64, f64)> {
    let mut all_rows: Vec<(i64, String, f64)> = Vec::new();
    for d in DAEMON_ROLES {
        if let Ok(rows) = state.store.recent_pnl_for(d, 5_000).await {
            for (ts, json) in rows {
                if let Some(v) = pnl_row_to_usd(&json) {
                    all_rows.push((ts, d.to_string(), v));
                }
            }
        }
    }
    all_rows.sort_by_key(|(ts, _, _)| *ts);
    let mut latest_per: std::collections::HashMap<String, f64> =
        std::collections::HashMap::new();
    let mut series: Vec<(i64, f64)> = Vec::new();
    for (ts, d, v) in all_rows {
        latest_per.insert(d, v);
        let total: f64 = latest_per.values().sum();
        series.push((ts, total));
    }
    series
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
