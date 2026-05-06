//! In-memory registry of positions the riskwatcher is observing.
//!
//! M2 scope: data-structure only. No network, no RPC. The registry is
//! later populated from two sources:
//!   - [`Source::Report`] — `ReportMultiply` envelopes received over the
//!     mesh (M3 observer).
//!   - [`Source::Poll`]   — Kamino obligation account polls via RPC
//!     (M4 poller).
//!
//! Capacity is hard-capped at [`REGISTRY_CAPACITY`] entries; on overflow
//! the oldest *insertion* is evicted (FIFO by insertion, NOT by update).
//! This means re-upserting the same subject does not refresh its
//! eviction order — a stuck-and-stale subject cannot squat the registry
//! forever just by getting Report-refreshed.
//!
//! Insertion order is tracked by a private monotonic counter
//! (`insertion_seq`) maintained inside the registry, NOT by the
//! caller-supplied `last_seen_unix`. This is robust against clock skew,
//! replayed `ReportMultiply` envelopes, and out-of-order timestamps:
//! the eviction-order key is decoupled from the data-observation
//! timestamp.

use std::collections::HashMap;

use solana_sdk::pubkey::Pubkey;
use tokio::sync::Mutex;

/// Maximum number of positions tracked simultaneously.
pub const REGISTRY_CAPACITY: usize = 32;

/// Where a [`PositionView`] entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Populated from a `ReportMultiply` envelope on the mesh.
    Report,
    /// Refreshed from a Kamino obligation account poll.
    Poll,
}

/// A single observed position. Public surface; M3/M4 build these and
/// pass them to [`ObservedPositions::upsert`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionView {
    /// Subject the multiply daemon is operating on (32-byte agent id).
    pub subject: [u8; 32],
    /// Kamino obligation account being tracked.
    pub obligation_pubkey: Pubkey,
    /// Last observed loan-to-value ratio, in basis points.
    pub last_ltv_bps: u16,
    /// UNIX timestamp (seconds) at which this view was last refreshed.
    pub last_seen_unix: u64,
    /// Whence this view came on its most recent refresh.
    pub source: Source,
}

/// Internal entry: the public view plus a private monotonic insertion
/// sequence number recording when the subject was *first* inserted.
/// This sequence is preserved across upserts — see module-level docs.
#[derive(Debug, Clone)]
struct Entry {
    view: PositionView,
    insertion_seq: u64,
}

/// Concurrency-safe registry of [`PositionView`]s, capped at
/// [`REGISTRY_CAPACITY`].
///
/// Multiple async tasks (e.g. the M3 observer and M4 poller) call
/// [`upsert`](Self::upsert) concurrently. The internal [`tokio::sync::Mutex`]
/// serialises them.
struct Inner {
    map: HashMap<[u8; 32], Entry>,
    /// Monotonically-increasing counter assigned to each new entry on
    /// insertion. Drives FIFO eviction order independent of any
    /// caller-supplied wall-clock timestamp.
    next_seq: u64,
}

pub struct ObservedPositions {
    inner: Mutex<Inner>,
}

impl Default for ObservedPositions {
    fn default() -> Self {
        Self::new()
    }
}

impl ObservedPositions {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::with_capacity(REGISTRY_CAPACITY),
                next_seq: 0,
            }),
        }
    }

    /// Insert or update a [`PositionView`].
    ///
    /// If `view.subject` is already present, the stored view is replaced
    /// but the original `insertion_seq` is preserved (so the entry's
    /// eviction priority does not change). If the subject is new and
    /// the registry is at capacity, the entry with the smallest
    /// `insertion_seq` is evicted before insertion.
    pub async fn upsert(&self, view: PositionView) {
        let mut guard = self.inner.lock().await;

        if let Some(existing) = guard.map.get_mut(&view.subject) {
            // Update preserves earliest seen — do NOT touch
            // insertion_seq.
            existing.view = view;
            return;
        }

        if guard.map.len() >= REGISTRY_CAPACITY {
            // Evict the oldest *insertion*. Ties are broken by subject
            // bytes for determinism — but with a single monotonic
            // counter under the same Mutex, ties are impossible. The
            // tie-break stays for defence-in-depth.
            if let Some(oldest_subject) = guard
                .map
                .iter()
                .min_by(|a, b| {
                    a.1.insertion_seq
                        .cmp(&b.1.insertion_seq)
                        .then_with(|| a.0.cmp(b.0))
                })
                .map(|(k, _)| *k)
            {
                guard.map.remove(&oldest_subject);
            }
        }

        let insertion_seq = guard.next_seq;
        guard.next_seq = guard.next_seq.wrapping_add(1);
        guard.map.insert(
            view.subject,
            Entry {
                view,
                insertion_seq,
            },
        );
    }

    /// Fetch a clone of the [`PositionView`] for `subject`, if present.
    pub async fn get(&self, subject: &[u8; 32]) -> Option<PositionView> {
        let guard = self.inner.lock().await;
        guard.map.get(subject).map(|e| e.view.clone())
    }

    /// Snapshot all currently-tracked views. Order is unspecified.
    pub async fn list(&self) -> Vec<PositionView> {
        let guard = self.inner.lock().await;
        guard.map.values().map(|e| e.view.clone()).collect()
    }

    /// Number of currently-tracked subjects.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.map.len()
    }

    /// Whether the registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.map.is_empty()
    }

    /// Test-only accessor: read the private `insertion_seq` for a
    /// subject. Used by integration tests to assert insertion-order
    /// stickiness across same-second updates.
    #[doc(hidden)]
    pub async fn __test_insertion_seq(&self, subject: &[u8; 32]) -> Option<u64> {
        let guard = self.inner.lock().await;
        guard.map.get(subject).map(|e| e.insertion_seq)
    }
}
