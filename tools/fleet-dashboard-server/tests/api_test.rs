//! API smoke tests via axum::Router::oneshot.
//!
//! These tests don't hit a real RPC; they exercise the endpoint
//! plumbing, query parsing, and SQLite-backed handlers. Chain-read
//! endpoints (`/aum`, `/positions`) are smoke-tested manually on Day 5
//! against the live mainnet wallet — those are flaky against unit tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

use fleet_dashboard_server::api::{router, AppState};
use fleet_dashboard_server::chain::ChainReader;
use fleet_dashboard_server::store::Store;
use fleet_dashboard_server::types::{Direction, MeshEvent};

async fn test_state(db_suffix: &str) -> AppState {
    let path = std::env::temp_dir().join(format!(
        "fds-api-{}-{}.sqlite",
        std::process::id(),
        db_suffix
    ));
    let _ = std::fs::remove_file(&path);
    let store = Arc::new(Store::open(&path).await.unwrap());
    let rpc_url = "https://api.mainnet-beta.solana.com".to_string();
    let chain = Arc::new(ChainReader::new(rpc_url.clone()));
    let (tx, _) = tokio::sync::broadcast::channel(64);
    AppState {
        store,
        chain,
        event_broadcast: tx,
        wallet_pubkey: solana_sdk::pubkey::Pubkey::new_unique(),
        rpc_url,
    }
}

fn ev(role: &str, msg_type: &str, ts_ms: i64) -> MeshEvent {
    MeshEvent {
        id: None,
        ts_unix: ts_ms / 1000,
        ts_ms,
        sender_role: role.to_string(),
        direction: Direction::Out,
        msg_type: msg_type.to_string(),
        payload_summary: "x".to_string(),
        payload_json: None,
        conv_id: None,
        tx_signature: None,
    }
}

#[tokio::test]
async fn events_endpoint_returns_empty_array_when_no_events() {
    let state = test_state("empty").await;
    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn events_endpoint_returns_inserted_events() {
    let state = test_state("inserted").await;
    state
        .store
        .insert_mesh_event(&ev("multiply", "Report", 1_714_000_000_000))
        .await
        .unwrap();
    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/events?limit=10")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
    assert_eq!(json[0]["sender_role"], "multiply");
    assert_eq!(json[0]["msg_type"], "Report");
}

#[tokio::test]
async fn events_endpoint_filters_by_role() {
    let state = test_state("filter-role").await;
    for role in ["multiply", "researcher", "researcher"] {
        state
            .store
            .insert_mesh_event(&ev(role, "MarketSignal", 1_714_000_000_000))
            .await
            .unwrap();
    }
    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/events?role=researcher")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().all(|e| e["sender_role"] == "researcher"));
}

#[tokio::test]
async fn events_endpoint_filters_by_msg_type() {
    let state = test_state("filter-type").await;
    state
        .store
        .insert_mesh_event(&ev("multiply", "Beacon", 1_714_000_000_000))
        .await
        .unwrap();
    state
        .store
        .insert_mesh_event(&ev("multiply", "Report", 1_714_000_001_000))
        .await
        .unwrap();
    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/events?type=Beacon")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["msg_type"], "Beacon");
}

#[tokio::test]
async fn daemons_endpoint_returns_unknown_for_silent_roles() {
    let state = test_state("daemons-silent").await;
    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/daemons")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 5);
    assert!(arr.iter().all(|d| d["status"] == "unknown"));
}

#[tokio::test]
async fn daemons_endpoint_marks_recent_beacon_as_green() {
    let state = test_state("daemons-green").await;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // Recent beacon for "multiply" should be green; others stay unknown.
    state
        .store
        .insert_mesh_event(&ev("multiply", "Beacon", now_ms - 1_000))
        .await
        .unwrap();
    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/daemons")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let arr = json.as_array().unwrap();
    let multiply = arr.iter().find(|d| d["role"] == "multiply").unwrap();
    assert_eq!(multiply["status"], "green");
    let other = arr.iter().find(|d| d["role"] == "researcher").unwrap();
    assert_eq!(other["status"], "unknown");
}

#[tokio::test]
async fn wallet_endpoint_returns_pubkey_and_balances() {
    let state = test_state("wallet").await;
    let expected_pubkey = state.wallet_pubkey.to_string();
    let expected_rpc = state.rpc_url.clone();
    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/wallet")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["pubkey"], expected_pubkey);
    assert_eq!(json["rpc_url"], expected_rpc);
    // Live RPC may or may not respond inside the test sandbox; either
    // way the field must be present and numeric. ATA-not-found falls
    // back to 0 in the balance reader, so 0 is the expected value for
    // a freshly-minted ephemeral pubkey.
    for key in ["sol_lamports", "usdc_lamports", "jlp_lamports"] {
        assert!(
            json[key].is_u64(),
            "{} should be numeric, got {:?}",
            key,
            json[key]
        );
    }
}

#[tokio::test]
async fn pnl_endpoint_returns_note_when_no_history() {
    let state = test_state("pnl-empty").await;
    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/pnl?window=24h")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["window"], "24h");
    assert_eq!(json["delta_usdc"], 0.0);
    assert!(json["note"].is_string());
}
