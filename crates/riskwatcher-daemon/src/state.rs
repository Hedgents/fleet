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
//! the oldest *insertion* is evicted (LRU on first-seen, NOT on update).
//! This means re-upserting the same subject does not refresh its
//! eviction order — a stuck-and-stale subject cannot squat the registry
//! forever just by getting Report-refreshed.

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

/// Internal entry: the public view plus a private LRU bookkeeping
/// timestamp recording when the subject was *first* inserted. This
/// timestamp is preserved across upserts — see module-level docs.
#[derive(Debug, Clone)]
struct Entry {
    view: PositionView,
    first_inserted_unix: u64,
}

/// Concurrency-safe registry of [`PositionView`]s, capped at
/// [`REGISTRY_CAPACITY`].
///
/// Multiple async tasks (e.g. the M3 observer and M4 poller) call
/// [`upsert`](Self::upsert) concurrently. The internal [`tokio::sync::Mutex`]
/// serialises them.
pub struct ObservedPositions {
    inner: Mutex<HashMap<[u8; 32], Entry>>,
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
            inner: Mutex::new(HashMap::with_capacity(REGISTRY_CAPACITY)),
        }
    }

    /// Insert or update a [`PositionView`].
    ///
    /// If `view.subject` is already present, the stored view is replaced
    /// but the original `first_inserted_unix` is preserved (so the
    /// entry's eviction priority does not change). If the subject is
    /// new and the registry is at capacity, the entry with the
    /// smallest `first_inserted_unix` is evicted before insertion.
    pub async fn upsert(&self, view: PositionView) {
        let mut guard = self.inner.lock().await;

        if let Some(existing) = guard.get_mut(&view.subject) {
            // Update preserves earliest seen — do NOT touch
            // first_inserted_unix.
            existing.view = view;
            return;
        }

        if guard.len() >= REGISTRY_CAPACITY {
            // Evict the oldest *insertion*. Ties are broken by subject
            // bytes for determinism (HashMap iteration order is not
            // stable across runs, but ties are vanishingly rare in
            // practice — a tie means two upserts landed on the same
            // wall-clock second).
            if let Some(oldest_subject) = guard
                .iter()
                .min_by(|a, b| {
                    a.1.first_inserted_unix
                        .cmp(&b.1.first_inserted_unix)
                        .then_with(|| a.0.cmp(b.0))
                })
                .map(|(k, _)| *k)
            {
                guard.remove(&oldest_subject);
            }
        }

        let first_inserted_unix = view.last_seen_unix;
        guard.insert(
            view.subject,
            Entry {
                view,
                first_inserted_unix,
            },
        );
    }

    /// Fetch a clone of the [`PositionView`] for `subject`, if present.
    pub async fn get(&self, subject: &[u8; 32]) -> Option<PositionView> {
        let guard = self.inner.lock().await;
        guard.get(subject).map(|e| e.view.clone())
    }

    /// Snapshot all currently-tracked views. Order is unspecified.
    pub async fn list(&self) -> Vec<PositionView> {
        let guard = self.inner.lock().await;
        guard.values().map(|e| e.view.clone()).collect()
    }

    /// Number of currently-tracked subjects.
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// Whether the registry is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }
}
