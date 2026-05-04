use axum::{routing::get, Json, Router};
use serde_json::json;

pub fn router(daemon_name: &'static str) -> Router {
    Router::new().route(
        "/health",
        get(move || async move {
            Json(json!({ "ok": true, "daemon": daemon_name }))
        }),
    )
}
