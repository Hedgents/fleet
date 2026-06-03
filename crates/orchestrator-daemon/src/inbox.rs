//! v0.4.14: orchestrator mesh-inbox loop.
//!
//! Pre-rc14 the orchestrator only EMITTED envelopes (Beacons, Assigns,
//! Withdraws). It never consumed anything — researcher signals were
//! broadcast to the execution daemons but never reached the
//! allocation-decision layer. This closes that loop:
//!
//!   1. Drain `handle.recv()` forever.
//!   2. Filter for `MsgType::MarketSignal`.
//!   3. Deserialise the CBOR payload via the existing `MarketSignal` shape.
//!   4. `MarketCache::upsert` — replacement is timestamp-ordered, so
//!      stale re-deliveries are silently dropped.
//!
//! No sender verification beyond what the mesh layer already enforces
//! at envelope decode time (signatures verified by `NodeService`).
//! Adding an explicit researcher-pubkey allowlist is rc15 work — for
//! now, the operational deploy controls who can broadcast onto the
//! mesh (single-operator, single keyring).
//!
//! Non-MarketSignal envelopes are dropped with a debug log. The
//! orchestrator does not currently consume Reports, Escalates, or
//! Beacons from other daemons — those flow into the dashboard's
//! ingest path, not here.

use anyhow::Result;
use tracing::{debug, info, warn};

use zerox1_node_enterprise::NodeHandle;
use zerox1_protocol::fleet::researcher::MarketSignal;
use zerox1_protocol::message::MsgType;

use crate::market_cache::SharedMarketCache;

pub async fn run(mut handle: NodeHandle, cache: SharedMarketCache) -> Result<()> {
    info!("orchestrator inbox loop starting (consuming MarketSignal)");
    while let Some(env) = handle.recv().await {
        match env.msg_type {
            MsgType::MarketSignal => {
                let signal: MarketSignal = match ciborium::de::from_reader(&env.payload[..]) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(
                            ?e,
                            sender = %hex::encode(env.sender),
                            "MarketSignal decode failed; dropping envelope"
                        );
                        continue;
                    }
                };
                let key_label = format!("{:?}/{:?}", signal.asset, signal.kind);
                let bps = signal.measurement_bps;
                let raised = signal.raised_at_unix;
                cache.write().await.upsert(signal);
                debug!(
                    key = %key_label,
                    measurement_bps = bps,
                    raised_at_unix = raised,
                    "MarketSignal cached"
                );
            }
            other => {
                debug!(
                    msg_type = ?other,
                    sender = %hex::encode(env.sender),
                    "envelope ignored — orchestrator only consumes MarketSignal"
                );
            }
        }
    }
    warn!("inbox channel closed; orchestrator inbox task exiting");
    anyhow::bail!("inbox channel closed")
}
