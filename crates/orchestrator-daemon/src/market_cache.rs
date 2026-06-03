//! v0.4.14: shared cache of latest `MarketSignal` per (asset, kind).
//!
//! The orchestrator subscribes to `MarketSignal` envelopes from
//! researcher (see `inbox.rs`) and updates this cache on each receipt.
//! The tick loop snapshots the cache once per tick and threads
//! relevant signals into the `AllocatorConfig` so allocation decisions
//! incorporate market context (e.g. don't propose a SOL-sale rebalance
//! when SOL is in a sharp downward move).
//!
//! Replacement semantics: latest-by-`raised_at_unix` wins. Older
//! signals for the same `(asset, kind)` are silently dropped. Cache
//! grows only with the cardinality of distinct (asset, kind) pairs
//! researcher emits — bounded in practice to a handful.
//!
//! No persistence: the cache lives entirely in-memory. On orchestrator
//! restart, the cache is empty until the next researcher emission
//! lands (typically within ~30s on the existing price watcher cadence).
//! Allocation gracefully degrades to v0.4.13 behaviour in the
//! warm-up window — a `None` signal means "no opinion."

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use zerox1_protocol::fleet::researcher::{AssetId, MarketSignal, SignalKind};

/// Key for cache lookups. `SignalKind` doesn't derive `Eq + Hash` in
/// the shared protocol crate, so we project it down to its `repr(u16)`
/// discriminant for HashMap use. AssetId already derives both.
pub type SignalKey = (AssetId, u16);

fn key(signal: &MarketSignal) -> SignalKey {
    (signal.asset, signal.kind as u16)
}

/// The cache itself — small `HashMap`, async-safe via `RwLock`.
#[derive(Default, Debug)]
pub struct MarketCache {
    entries: HashMap<SignalKey, MarketSignal>,
}

impl MarketCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replacement policy: keep the higher `raised_at_unix`. Older
    /// signals (mesh delivery reorder, late re-broadcast) silently drop.
    pub fn upsert(&mut self, signal: MarketSignal) {
        let k = key(&signal);
        match self.entries.get(&k) {
            Some(prior) if prior.raised_at_unix >= signal.raised_at_unix => {
                // Stale or duplicate — drop.
            }
            _ => {
                self.entries.insert(k, signal);
            }
        }
    }

    /// Read the latest signal for `(asset, kind)`. Returns `None` when
    /// nothing has landed yet (cold cache) — callers treat that as
    /// "no opinion" and fall through to v0.4.13 behaviour.
    pub fn get(&self, asset: AssetId, kind: SignalKind) -> Option<&MarketSignal> {
        self.entries.get(&(asset, kind as u16))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub type SharedMarketCache = Arc<RwLock<MarketCache>>;

pub fn new_shared() -> SharedMarketCache {
    Arc::new(RwLock::new(MarketCache::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerox1_protocol::fleet::researcher::SignalSeverity;

    fn signal(asset: AssetId, kind: SignalKind, ts: u64, bps: i32) -> MarketSignal {
        MarketSignal {
            kind,
            asset,
            asset_mint: [0u8; 32],
            measurement_bps: bps,
            severity: SignalSeverity::Notice,
            raised_at_unix: ts,
            context_value: 0,
        }
    }

    #[test]
    fn upsert_replaces_older() {
        let mut c = MarketCache::new();
        c.upsert(signal(AssetId::SOL, SignalKind::PriceMovedBps, 100, -200));
        c.upsert(signal(AssetId::SOL, SignalKind::PriceMovedBps, 200, -500));
        let v = c
            .get(AssetId::SOL, SignalKind::PriceMovedBps)
            .expect("present");
        assert_eq!(v.raised_at_unix, 200);
        assert_eq!(v.measurement_bps, -500);
    }

    #[test]
    fn upsert_keeps_newer_when_stale_arrives() {
        let mut c = MarketCache::new();
        c.upsert(signal(AssetId::SOL, SignalKind::PriceMovedBps, 200, -500));
        c.upsert(signal(AssetId::SOL, SignalKind::PriceMovedBps, 100, -200));
        let v = c
            .get(AssetId::SOL, SignalKind::PriceMovedBps)
            .expect("present");
        assert_eq!(v.raised_at_unix, 200);
        assert_eq!(v.measurement_bps, -500);
    }

    #[test]
    fn distinct_keys_coexist() {
        let mut c = MarketCache::new();
        c.upsert(signal(AssetId::SOL, SignalKind::PriceMovedBps, 100, -200));
        c.upsert(signal(AssetId::SOL, SignalKind::LendingBorrowRateAbove, 100, 1500));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn missing_key_returns_none() {
        let c = MarketCache::new();
        assert!(c.get(AssetId::SOL, SignalKind::PriceMovedBps).is_none());
    }
}
