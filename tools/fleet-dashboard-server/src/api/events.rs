//! `GET /events` and `WS /events/live`.
//!
//! `GET /events?since=<unix_ms>&limit=<usize>&role=<filter>&type=<filter>`:
//!   returns mesh history newest-first.
//!
//! `WS /events/live`: streams every new MeshEvent as it lands. On
//! connect, sends the last 50 events (chronological-then-newest-last)
//! before subscribing to the broadcast channel.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::api::AppState;
use crate::types::MeshEvent;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 1000;
const LIVE_REPLAY: usize = 50;

#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    pub since: Option<i64>,
    pub limit: Option<usize>,
    pub role: Option<String>,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    /// Exclude Beacon heartbeats from results. Defaults to true — health
    /// pills already surface Beacons; the feed only needs actionable events.
    pub exclude_beacons: Option<bool>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/events", get(get_events))
        .route("/events/activity", get(get_activity))
        .route("/events/live", get(ws_handler))
}

#[derive(Debug, Deserialize)]
pub struct ActivityQuery {
    /// How many hours back to bucket. Defaults to 24; capped at 168 (one week).
    pub hours: Option<u32>,
}

#[derive(serde::Serialize)]
struct ActivityBucket {
    /// Unix milliseconds at the start of the bucket (oldest-first).
    ts_ms: i64,
    /// Count of non-Beacon mesh events that fell in this bucket.
    events: u64,
}

async fn get_activity(
    State(state): State<AppState>,
    Query(q): Query<ActivityQuery>,
) -> impl IntoResponse {
    let hours = q.hours.unwrap_or(24).clamp(1, 168);
    match state.store.activity_buckets_ms(hours).await {
        Ok(rows) => {
            let buckets: Vec<ActivityBucket> = rows
                .into_iter()
                .map(|(ts_ms, events)| ActivityBucket { ts_ms, events })
                .collect();
            Json(buckets).into_response()
        }
        Err(e) => {
            tracing::warn!(?e, "/events/activity query failed");
            Json(Vec::<ActivityBucket>::new()).into_response()
        }
    }
}

async fn get_events(
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT).max(1);
    let since = q.since.unwrap_or(0);
    let role = q.role.as_deref();
    let msg_type = q.msg_type.as_deref();
    // When the caller provides an explicit msg_type filter they get exactly
    // what they asked for — don't also exclude Beacon rows, otherwise
    // `?type=Beacon` returns nothing.
    let exclude_beacons = q.exclude_beacons.unwrap_or(msg_type.is_none());
    match state
        .store
        .recent_events_filtered(since, limit, role, msg_type, exclude_beacons)
        .await
    {
        Ok(events) => Json(events).into_response(),
        Err(e) => {
            tracing::warn!(?e, "/events query failed");
            Json(Vec::<MeshEvent>::new()).into_response()
        }
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| live_socket(socket, state))
}

async fn live_socket(mut socket: WebSocket, state: AppState) {
    // Initial replay: last 50 non-Beacon events, oldest-first so the client lands sorted.
    if let Ok(mut recent) = state
        .store
        .recent_events_filtered(0, LIVE_REPLAY, None, None, true)
        .await
    {
        recent.reverse();
        for evt in recent {
            let Ok(text) = serde_json::to_string(&evt) else {
                continue;
            };
            if socket.send(Message::Text(text)).await.is_err() {
                return;
            }
        }
    }

    let mut rx = state.event_broadcast.subscribe();
    loop {
        match rx.recv().await {
            Ok(evt) => {
                let Ok(text) = serde_json::to_string(&evt) else {
                    continue;
                };
                if socket.send(Message::Text(text)).await.is_err() {
                    return;
                }
            }
            Err(RecvError::Lagged(_)) => {
                // Slow consumer dropped events; keep going on the
                // freshest data the broadcast channel has.
                continue;
            }
            Err(RecvError::Closed) => return,
        }
    }
}
