//! Boot-time chain replay.
//!
//! Each daemon implements `ChainReplay::replay(...)` to reconstruct in-
//! memory state from on-chain ixns matching its role's signing key. This
//! is what makes daemons fungible: kill any process, start a replacement
//! on a different host with the same role key, replay rebuilds the state
//! in seconds — no shared local storage required.
//!
//! Concrete implementations live in each daemon crate's strategy module
//! (filled in by per-daemon strategy follow-up plans). The runtime crate
//! defines only the contract.

use anyhow::Result;
use async_trait::async_trait;
use solana_sdk::signature::Signature;

use crate::identity::Role;
use crate::rpc::RpcContext;

/// A daemon-specific state object built from chain history.
///
/// The trait method scans Solana history filtered by the role's signing
/// pubkey, decodes the relevant ixns into the daemon's canonical
/// in-memory representation, and returns it. After boot, the daemon
/// keeps the replayed state hot via incremental updates anchored at
/// `last_signature()`.
#[async_trait]
pub trait ChainReplay: Sized + Send {
    /// Restore state from on-chain history. Implementors typically:
    ///   1. Resolve the role's signing pubkey via `runtime::identity`.
    ///   2. Use `rpc.client.get_signatures_for_address(...)` to page
    ///      back to the last N hours of activity.
    ///   3. Fetch full transactions for each signature and decode the
    ///      ixns into the daemon's typed state shape.
    ///   4. Return the rebuilt state.
    ///
    /// Errors should bubble up — if replay fails the daemon must NOT
    /// boot into an unknown state. Boot-failure is acceptable; partial
    /// or speculative state is not.
    async fn replay(rpc: &RpcContext, role: Role) -> Result<Self>;

    /// Most recently observed signature in the replayed history.
    /// Used as the anchor for incremental updates after boot.
    /// Returns `None` for a fresh fleet (no history yet).
    fn last_signature(&self) -> Option<Signature>;
}
