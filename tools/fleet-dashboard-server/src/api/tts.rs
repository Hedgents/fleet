//! `GET /tts?text=...` — thin proxy to ElevenLabs text-to-speech.
//!
//! Used by the dashboard's hourly-briefing component to voice a one-
//! sentence portfolio summary. The dashboard composes the script
//! client-side (it already has /paper + /rates data); this endpoint
//! just forwards the text to ElevenLabs and streams the resulting MP3
//! back. Keeps the API key on the server.
//!
//! Disabled (HTTP 503) when `ELEVENLABS_API_KEY` is not set.

use axum::extract::Query;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;

use crate::api::AppState;

/// Default voice — "Rachel", a neutral analyst-tone voice. Operators
/// can override per-request with `?voice_id=...`.
const DEFAULT_VOICE_ID: &str = "21m00Tcm4TlvDq8ikWAM";
/// Fast model — ~500ms latency, good enough for hourly briefings.
const MODEL_ID: &str = "eleven_turbo_v2_5";
/// Bound the text the proxy will forward to keep cost predictable.
const MAX_TEXT_CHARS: usize = 600;

#[derive(Debug, Deserialize)]
pub struct TtsQuery {
    pub text: String,
    #[serde(default)]
    pub voice_id: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/tts", get(tts))
}

async fn tts(Query(q): Query<TtsQuery>) -> Response {
    let Ok(api_key) = std::env::var("ELEVENLABS_API_KEY") else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "ELEVENLABS_API_KEY not configured on the dashboard server",
        )
            .into_response();
    };
    if q.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "text query param required").into_response();
    }
    if q.text.len() > MAX_TEXT_CHARS {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("text exceeds {MAX_TEXT_CHARS} chars"),
        )
            .into_response();
    }

    let voice_id = q.voice_id.as_deref().unwrap_or(DEFAULT_VOICE_ID);
    let url = format!("https://api.elevenlabs.io/v1/text-to-speech/{voice_id}");
    let body = serde_json::json!({
        "text": q.text,
        "model_id": MODEL_ID,
        "voice_settings": { "stability": 0.4, "similarity_boost": 0.75 },
    });

    let client = reqwest::Client::new();
    let resp = match client
        .post(&url)
        .header("xi-api-key", api_key)
        .header(header::CONTENT_TYPE, "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(?e, "ElevenLabs request failed");
            return (StatusCode::BAD_GATEWAY, "ElevenLabs request failed").into_response();
        }
    };

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(?status, %body, "ElevenLabs returned non-2xx");
        return (StatusCode::BAD_GATEWAY, format!("ElevenLabs: {status}")).into_response();
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(?e, "ElevenLabs body read failed");
            return (StatusCode::BAD_GATEWAY, "ElevenLabs body read failed").into_response();
        }
    };

    ([(header::CONTENT_TYPE, "audio/mpeg")], bytes).into_response()
}
