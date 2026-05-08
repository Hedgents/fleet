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
        .route("/events/live", get(ws_handler))
}

async fn get_events(
    State(state): State<AppState>,
    Query(q): Query<EventsQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT).max(1);
    let since = q.since.unwrap_or(0);
    let role = q.role.as_deref();
    let msg_type = q.msg_type.as_deref();
    let exclude_beacons = q.exclude_beacons.unwrap_or(true);
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
