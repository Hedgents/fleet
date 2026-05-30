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

#[tokio::test]
async fn apr_samples_round_trip_and_filter_by_strategy_and_hours() {
    let path = std::env::temp_dir().join(format!(
        "fds-apr-test-{}-{}.sqlite",
        std::process::id(),
        rand_suffix()
    ));
    let _ = std::fs::remove_file(&path);
    let store = Store::open(&path).await.unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    // Two strategies, several samples each. One sample is older than the
    // 1h window and must be filtered out.
    store
        .insert_apr_sample(now_ms - 600_000, "stable_yield", 940, "kamino_reserve")
        .await
        .unwrap();
    store
        .insert_apr_sample(now_ms - 300_000, "stable_yield", 950, "kamino_reserve")
        .await
        .unwrap();
    store
        .insert_apr_sample(now_ms - 60_000, "stable_yield", 960, "kamino_reserve")
        .await
        .unwrap();
    // Outside the 1h window:
    store
        .insert_apr_sample(now_ms - 7_200_000, "stable_yield", 100, "kamino_reserve")
        .await
        .unwrap();
    // Different strategy:
    store
        .insert_apr_sample(now_ms - 60_000, "multiply", 1234, "daemon_telemetry")
        .await
        .unwrap();

    let s = store.apr_samples_for("stable_yield", 1).await.unwrap();
    assert_eq!(s.len(), 3, "1h window should exclude 2h-old sample");
    // Oldest first.
    assert_eq!(s[0].1, 940);
    assert_eq!(s[2].1, 960);

    let m = store.apr_samples_for("multiply", 24).await.unwrap();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].1, 1234);

    let _ = std::fs::remove_file(&path);
}

// rc24: chain-AUM snapshot round-trip tests.

#[tokio::test]
async fn chain_aum_snapshot_round_trips() {
    let path = std::env::temp_dir().join(format!(
        "fds-aum-test-{}-{}.sqlite",
        std::process::id(),
        rand_suffix()
    ));
    let _ = std::fs::remove_file(&path);

    let store = Store::open(&path).await.unwrap();
    // Three snapshots at 0, 30s, 60s.
    store
        .insert_chain_aum_snapshot(
            1_000_000, 200.0, 50.0, 30.0, 100.0, 20.0, 0.0, None, None, None, None, None,
        )
        .await
        .unwrap();
    store
        .insert_chain_aum_snapshot(
            1_000_030, 201.0, 50.1, 30.1, 100.5, 20.1, 0.2, None, None, None, None, None,
        )
        .await
        .unwrap();
    store
        .insert_chain_aum_snapshot(
            1_000_060, 202.0, 50.2, 30.2, 101.0, 20.2, 0.4, None, None, None, None, None,
        )
        .await
        .unwrap();

    // since(cutoff 999_999) returns all three, oldest-first.
    let rows = store.chain_aum_snapshots_since(999_999).await.unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].ts_unix, 1_000_000);
    assert_eq!(rows[2].ts_unix, 1_000_060);
    // Per-strategy fields preserved through the round trip.
    assert!((rows[0].multiply_usd - 50.0).abs() < 1e-9);
    assert!((rows[2].hedgedjlp_jlp_usd - 101.0).abs() < 1e-9);
    assert!((rows[2].hedgedjlp_collateral_usd - 20.2).abs() < 1e-9);

    // since(cutoff after first snapshot) excludes the oldest row.
    let rows = store.chain_aum_snapshots_since(1_000_001).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].ts_unix, 1_000_030);

    assert_eq!(store.chain_aum_snapshot_count().await.unwrap(), 3);

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn chain_aum_snapshot_ignores_duplicate_ts() {
    // ON CONFLICT (ts_unix) DO NOTHING: a second insert at the same
    // ts_unix must be a no-op (not an error). This is the contract the
    // sampler relies on when it races a hand-call at boot.
    let path = std::env::temp_dir().join(format!(
        "fds-aum-dup-{}-{}.sqlite",
        std::process::id(),
        rand_suffix()
    ));
    let _ = std::fs::remove_file(&path);

    let store = Store::open(&path).await.unwrap();
    store
        .insert_chain_aum_snapshot(
            1_000_000, 200.0, 50.0, 30.0, 100.0, 20.0, 0.0, None, None, None, None, None,
        )
        .await
        .unwrap();
    // Second insert at the same ts → no error, no duplicate row.
    store
        .insert_chain_aum_snapshot(
            1_000_000, 999.0, 50.0, 30.0, 100.0, 20.0, 0.0, None, None, None, None, None,
        )
        .await
        .unwrap();

    assert_eq!(store.chain_aum_snapshot_count().await.unwrap(), 1);
    // First write wins (ON CONFLICT DO NOTHING).
    let rows = store.chain_aum_snapshots_since(0).await.unwrap();
    assert!((rows[0].total_usd - 200.0).abs() < 1e-9);

    let _ = std::fs::remove_file(&path);
}

fn rand_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

// ── v0.4.1: beta vault share math ───────────────────────────────────────────

/// End-to-end pro-rata sanity check: founder seeds $260, depositor adds
/// $100 at NAV=1.0, AUM later grows 10% → both holders share earnings
/// pro-rata to share count.
#[tokio::test]
async fn beta_vault_pro_rata_math_round_trips() {
    let path = std::env::temp_dir().join(format!(
        "fds-test-beta-{}-{}.sqlite",
        std::process::id(),
        rand_suffix()
    ));
    let _ = std::fs::remove_file(&path);
    let store = Store::open(&path).await.unwrap();

    // Founder seed: $260 USDC at NAV=1.00 → 260M micro-USDC, 260M share-lamports.
    let aum_seed_micro = 260_000_000_i64;
    store.record_founder_seed(aum_seed_micro).await.unwrap();
    assert_eq!(
        store.total_shares_outstanding().await.unwrap(),
        aum_seed_micro
    );

    // Depositor adds $100. NAV is still 1.00 (no earnings yet), so they
    // should mint 100M share-lamports (1:1 with USDC at NAV=1).
    let dep_amount = 100_000_000_i64;
    let aum_at_deposit = aum_seed_micro; // pre-deposit
    let minted = store
        .record_beta_deposit(
            "Depositor1ABC...",
            Some("dep1@example.com"),
            Some("alpha-001"),
            dep_amount,
            aum_at_deposit,
            None,
            Some("first beta deposit"),
        )
        .await
        .unwrap();
    assert_eq!(minted, dep_amount, "NAV=1 → 1 USDC mints 1 share");

    // Total shares = 360M. Fleet earns 10% → AUM = $260+$100 = $360 ×
    // 1.10 = $396. NAV = 396M × 1e6 / 360M = 1.1M micro = $1.10.
    let aum_after_earnings = ((aum_seed_micro + dep_amount) as i128 * 11 / 10) as i64;
    assert_eq!(aum_after_earnings, 396_000_000);

    // Depositor1 redeems half their $100. At NAV=1.10 → $55.
    // Shares burned = $55 × 360M / 396M = 50M share-lamports (half of 100M).
    let withdraw_amount = 55_000_000;
    let burned = store
        .record_beta_withdrawal(
            "Depositor1ABC...",
            "Depositor1DESTxyz...",
            withdraw_amount,
            aum_after_earnings,
            None,
            None,
        )
        .await
        .unwrap();
    assert_eq!(burned, 50_000_000, "half their shares burned at NAV=1.10");

    // Remaining shares: 260M (founder) + 50M (depositor remains) = 310M.
    assert_eq!(store.total_shares_outstanding().await.unwrap(), 310_000_000);

    let positions = store.list_beta_positions().await.unwrap();
    assert_eq!(positions.len(), 2);
    let depositor = positions
        .iter()
        .find(|p| p.source_address == "Depositor1ABC...")
        .expect("depositor row present");
    assert_eq!(depositor.shares_lamports, 50_000_000);
    assert_eq!(depositor.total_deposited_usdc_lamports, dep_amount);
    assert_eq!(depositor.total_withdrawn_usdc_lamports, withdraw_amount);

    let _ = std::fs::remove_file(&path);
}

/// Idempotency: re-running the founder seed does not double-mint.
#[tokio::test]
async fn beta_vault_founder_seed_idempotent() {
    let path = std::env::temp_dir().join(format!(
        "fds-test-beta-{}-{}.sqlite",
        std::process::id(),
        rand_suffix()
    ));
    let _ = std::fs::remove_file(&path);
    let store = Store::open(&path).await.unwrap();
    store.record_founder_seed(100_000_000).await.unwrap();
    // Second call with a different AUM should NOT mint new shares.
    store.record_founder_seed(999_000_000).await.unwrap();
    assert_eq!(
        store.total_shares_outstanding().await.unwrap(),
        100_000_000,
        "re-seed must be a no-op"
    );
    let _ = std::fs::remove_file(&path);
}

/// Sanity: deposit before seed fails clearly rather than silently
/// minting infinite shares (division-by-zero on total_shares).
#[tokio::test]
async fn beta_vault_deposit_before_seed_errors() {
    let path = std::env::temp_dir().join(format!(
        "fds-test-beta-{}-{}.sqlite",
        std::process::id(),
        rand_suffix()
    ));
    let _ = std::fs::remove_file(&path);
    let store = Store::open(&path).await.unwrap();
    let r = store
        .record_beta_deposit("Foo", None, None, 10_000_000, 0, None, None)
        .await;
    assert!(r.is_err(), "deposit before founder seed must error");
    let _ = std::fs::remove_file(&path);
}
