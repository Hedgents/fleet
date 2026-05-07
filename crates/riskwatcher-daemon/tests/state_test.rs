//! Integration tests for the M2 ObservedPositions registry.
//!
//! Verifies:
//!   1. insert + get round-trip
//!   2. update preserves first-inserted (eviction order is sticky)
//!   3. eviction at capacity removes the oldest insertion
//!   4. concurrent upserts are safe and capacity-bounded
//!   5. update at exact capacity boundary does NOT evict
//!   6. same-subject double-upsert in the same wall-second preserves
//!      both the latest data fields AND the original insertion_seq

use std::sync::Arc;

use solana_sdk::pubkey::Pubkey;

use riskwatcher_daemon::state::{ObservedPositions, PositionView, Source, REGISTRY_CAPACITY};

fn subject(byte: u8) -> [u8; 32] {
    [byte; 32]
}

fn view(byte: u8, last_ltv_bps: u16, last_seen_unix: u64, source: Source) -> PositionView {
    PositionView {
        subject: subject(byte),
        obligation_pubkey: Pubkey::new_unique(),
        last_ltv_bps,
        last_seen_unix,
        source,
    }
}

#[tokio::test]
async fn insert_and_get() {
    let reg = ObservedPositions::new();
    let v = view(1, 4_000, 100, Source::Report);
    reg.upsert(v.clone()).await;

    let got = reg.get(&subject(1)).await.expect("entry should exist");
    assert_eq!(got, v);
    assert_eq!(reg.len().await, 1);
}

#[tokio::test]
async fn update_preserves_first_inserted() {
    // Spec scenario: insert subject X first, then 31 other subjects; update
    // X (its `last_seen_unix` is now newer than every other entry's
    // first-inserted timestamp). Upsert one new subject Y, forcing
    // eviction. X must be evicted — the update must NOT have refreshed
    // its eviction order.
    let reg = ObservedPositions::new();

    // X first, at t=100, ltv=4000.
    reg.upsert(view(0xAA, 4_000, 100, Source::Report)).await;

    // 31 other subjects at t=200..230 — all newer first-inserted than X.
    for i in 0..(REGISTRY_CAPACITY as u8 - 1) {
        // Subject bytes 0x10..0x2E — distinct from 0xAA and 0xBB.
        reg.upsert(view(0x10 + i, 5_000, 200 + i as u64, Source::Report))
            .await;
    }
    assert_eq!(reg.len().await, REGISTRY_CAPACITY);

    // Update X with a fresh `last_seen_unix=999`. If update wrongly
    // refreshed first_inserted, X would survive the next eviction.
    reg.upsert(view(0xAA, 6_000, 999, Source::Poll)).await;
    let updated = reg.get(&subject(0xAA)).await.expect("X still present");
    assert_eq!(updated.last_ltv_bps, 6_000);
    assert_eq!(updated.last_seen_unix, 999);
    assert_eq!(updated.source, Source::Poll);

    // Overflow with subject Y.
    reg.upsert(view(0xBB, 5_500, 1_000, Source::Report)).await;

    // X must be the evicted entry; Y must be present; len() == capacity.
    assert!(
        reg.get(&subject(0xAA)).await.is_none(),
        "X should have been evicted because update preserved its earliest first-inserted"
    );
    assert!(
        reg.get(&subject(0xBB)).await.is_some(),
        "Y should be present after overflow upsert"
    );
    assert_eq!(reg.len().await, REGISTRY_CAPACITY);
}

#[tokio::test]
async fn eviction_at_capacity() {
    let reg = ObservedPositions::new();

    // Fill to capacity. Subject byte i is inserted at t = 100 + i.
    for i in 0..REGISTRY_CAPACITY as u8 {
        reg.upsert(view(i, 4_500, 100 + i as u64, Source::Report)).await;
    }
    assert_eq!(reg.len().await, REGISTRY_CAPACITY);

    // The oldest insertion is subject byte 0 (t=100). Add a 33rd entry.
    let new_byte = REGISTRY_CAPACITY as u8; // 32
    reg.upsert(view(new_byte, 4_500, 1_000, Source::Poll)).await;

    assert!(
        reg.get(&subject(0)).await.is_none(),
        "oldest first-inserted entry (byte 0) should have been evicted"
    );
    assert!(
        reg.get(&subject(new_byte)).await.is_some(),
        "newly-inserted overflow entry should be present"
    );
    assert_eq!(reg.len().await, REGISTRY_CAPACITY);
}

#[tokio::test]
async fn update_at_exact_capacity_does_not_evict() {
    // Fill registry to exactly REGISTRY_CAPACITY entries, then update an
    // existing subject. The upsert must hit the update branch (not
    // insert-with-overflow), so len() stays at capacity AND every
    // pre-existing subject is still present.
    let reg = ObservedPositions::new();

    for i in 0..REGISTRY_CAPACITY as u8 {
        reg.upsert(view(i, 4_000, 100 + i as u64, Source::Report)).await;
    }
    assert_eq!(reg.len().await, REGISTRY_CAPACITY);

    // Update one of the existing 32 subjects (pick the middle one).
    let target = REGISTRY_CAPACITY as u8 / 2;
    reg.upsert(view(target, 7_500, 999, Source::Poll)).await;

    // No eviction: len unchanged, target reflects the update.
    assert_eq!(reg.len().await, REGISTRY_CAPACITY);
    let updated = reg.get(&subject(target)).await.expect("target still present");
    assert_eq!(updated.last_ltv_bps, 7_500);
    assert_eq!(updated.last_seen_unix, 999);
    assert_eq!(updated.source, Source::Poll);

    // Every other original subject is also still present.
    for i in 0..REGISTRY_CAPACITY as u8 {
        assert!(
            reg.get(&subject(i)).await.is_some(),
            "subject {i} should still be present after at-capacity update"
        );
    }
}

#[tokio::test]
async fn same_second_double_upsert_preserves_seq() {
    // Two upserts of subject X within the same wall-second: the second
    // upsert must overwrite ltv but leave last_seen_unix untouched at
    // its supplied value AND leave the internal insertion_seq unchanged
    // (insertion order is sticky on update — independent of clock).
    let reg = ObservedPositions::new();

    reg.upsert(view(0xCC, 4_000, 100, Source::Report)).await;
    let seq_after_first = reg
        .__test_insertion_seq(&subject(0xCC))
        .await
        .expect("X present after first upsert");

    reg.upsert(view(0xCC, 5_000, 100, Source::Poll)).await;

    let updated = reg.get(&subject(0xCC)).await.expect("X still present");
    assert_eq!(updated.last_ltv_bps, 5_000);
    assert_eq!(updated.last_seen_unix, 100);
    assert_eq!(updated.source, Source::Poll);

    let seq_after_second = reg
        .__test_insertion_seq(&subject(0xCC))
        .await
        .expect("X still present after second upsert");
    assert_eq!(
        seq_after_first, seq_after_second,
        "insertion_seq must be sticky across same-subject re-upserts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_upsert_safe() {
    let reg = Arc::new(ObservedPositions::new());

    // Spawn 64 tasks, each inserting a unique subject. After all complete,
    // len() must be exactly REGISTRY_CAPACITY (= 32) — half were evicted —
    // and every surviving entry must be a well-formed PositionView.
    let mut handles = Vec::with_capacity(64);
    for i in 0..64u32 {
        let reg = Arc::clone(&reg);
        handles.push(tokio::spawn(async move {
            // Subject differentiated by byte 0..31 for low half, 32..63 for high.
            let mut s = [0u8; 32];
            s[0] = (i & 0xFF) as u8;
            s[1] = ((i >> 8) & 0xFF) as u8;
            let v = PositionView {
                subject: s,
                obligation_pubkey: Pubkey::new_unique(),
                last_ltv_bps: (i % 10_000) as u16,
                last_seen_unix: 1_000 + i as u64,
                source: if i % 2 == 0 { Source::Report } else { Source::Poll },
            };
            reg.upsert(v).await;
        }));
    }

    for h in handles {
        h.await.expect("task panicked");
    }

    let len = reg.len().await;
    assert_eq!(
        len, REGISTRY_CAPACITY,
        "registry must be exactly capacity-bounded after concurrent upserts (got {len})"
    );

    let all = reg.list().await;
    assert_eq!(all.len(), REGISTRY_CAPACITY);
    for v in all {
        // Sanity: every survivor is a valid view (non-zero last_seen_unix,
        // ltv in band).
        assert!(v.last_seen_unix >= 1_000);
        assert!(v.last_ltv_bps < 10_000);
    }
}
