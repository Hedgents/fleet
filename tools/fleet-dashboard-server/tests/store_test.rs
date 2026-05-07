//! SQLite store round-trip tests.

use fleet_dashboard_server::store::Store;
use fleet_dashboard_server::types::{Direction, MeshEvent};

#[tokio::test]
async fn open_creates_schema_and_inserts_event() {
    let path = std::env::temp_dir().join(format!(
        "fds-test-{}-{}.sqlite",
        std::process::id(),
        rand_suffix()
    ));
    let _ = std::fs::remove_file(&path);

    let store = Store::open(&path).await.unwrap();
    let event = MeshEvent {
        id: None,
        ts_unix: 1_714_000_000,
        ts_ms: 1_714_000_000_123,
        sender_role: "multiply".to_string(),
        direction: Direction::Out,
        msg_type: "Report".to_string(),
        payload_summary: "test report".to_string(),
        payload_json: Some("{}".to_string()),
        conv_id: Some("abc123".to_string()),
        tx_signature: None,
    };
    let id = store.insert_mesh_event(&event).await.unwrap();
    assert!(id > 0);

    let all = store.recent_events(0, 100).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].sender_role, "multiply");
    assert_eq!(all[0].direction, Direction::Out);

    let n = store.event_count().await.unwrap();
    assert_eq!(n, 1);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn insert_pnl_snapshot_round_trips() {
    let path = std::env::temp_dir().join(format!(
        "fds-pnl-test-{}-{}.sqlite",
        std::process::id(),
        rand_suffix()
    ));
    let _ = std::fs::remove_file(&path);

    let store = Store::open(&path).await.unwrap();
    let id = store
        .insert_pnl_snapshot(
            "multiply",
            1_714_000_000,
            r#"{"ts":1714000000,"current_ltv_bps":7000}"#,
        )
        .await
        .unwrap();
    assert!(id > 0);

    let _ = std::fs::remove_file(&path);
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}
