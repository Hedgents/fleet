//! Watchers — periodic async tasks that read on-chain state and emit
//! MarketSignal envelopes when thresholds cross. Each watcher is
//! independent; they share a single `EmissionTracker` for de-dup.

pub mod jlp_yield;
pub mod lending_rate;
pub mod perp_funding;
pub mod price;
pub mod stable_peg;
pub mod token_activity;
