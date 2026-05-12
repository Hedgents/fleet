//! End-to-end ingest test: synthetic JSONL line -> log_tailer ->
//! decoder -> store.
//!
//! This test is timing-sensitive on macOS because notify's FSEvents
//! backend can buffer events for hundreds of milliseconds. If it flakes
//! in CI or on slower laptops, mark it `#[ignore]` — the unit tests
//! cover the decoder exhaustively.

use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn end_to_end_log_to_store() {
    use fleet_dashboard_server::ingest::{envelope_decoder, log_tailer};
    use fleet_dashboard_server::store::Store;
    use tokio::sync::mpsc;

    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("fds-e2e-{}-{}", std::process::id(), suffix));
    std::fs::create_dir_all(&dir).unwrap();
    let log_path = dir.join("hedgedjlp.log");
    tokio::fs::write(&log_path, b"").await.unwrap();

    let db_path = dir.join("test.sqlite");
    let _ = std::fs::remove_file(&db_path);
    let store = Arc::new(Store::open(&db_path).await.unwrap());

    let (tx, mut rx) = mpsc::channel(128);
    let dir_clone = dir.clone();
    let tailer_handle = tokio::spawn(async move { log_tailer::run(dir_clone, tx).await });

    let store_clone = store.clone();
    let decoder_handle = tokio::spawn(async move {
        while let Some(raw) = rx.recv().await {
            if let Some(event) = envelope_decoder::decode_log_line(&raw) {
                let _ = store_clone.insert_mesh_event(&event).await;
            }
        }
    });

    // Let the tailer install its watcher and finish the initial sweep.
    sleep(Duration::from_millis(300)).await;

    let mut f = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&log_path)
        .await
        .unwrap();
    let line = serde_json::json!({
        "timestamp": "2026-05-06T14:23:07.123Z",
        "level": "INFO",
        "fields": {"message": "BEACON emitted", "role": "hedgedjlp", "nonce": 1},
        "target": "hedgedjlp_daemon::main",
    });
    f.write_all((line.to_string() + "\n").as_bytes())
        .await
        .unwrap();
    f.flush().await.unwrap();
    drop(f);

    // Poll up to 3s for the event to arrive — notify on macOS can be
    // bursty.
    let mut count = 0;
    for _ in 0..30 {
        sleep(Duration::from_millis(100)).await;
        count = store.event_count().await.unwrap();
        if count >= 1 {
            break;
        }
    }
    assert!(
        count >= 1,
        "expected at least 1 event ingested, got {}",
        count
    );

    tailer_handle.abort();
    decoder_handle.abort();
    let _ = std::fs::remove_dir_all(&dir);
}
