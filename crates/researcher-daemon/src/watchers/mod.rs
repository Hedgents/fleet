//! Watchers — periodic async tasks that read on-chain state and emit
//! MarketSignal envelopes when thresholds cross. Each watcher is
//! independent; they share a single `EmissionTracker` for de-dup.

pub mod lending_rate;
