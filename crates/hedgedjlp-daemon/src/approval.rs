//! Manual-approval queue.
//!
//! When `require_approval=true` (default on mainnet), the dispatch
//! does NOT execute incoming Assigns/Withdraws immediately. Instead, it
//! stores the (conversation_id, payload, sender_pubkey) tuple, emits
//! an Escalate(Notice, NeedsApproval) envelope to the orchestrator,
//! and waits for an Approve envelope referencing the same conv_id
//! AND signed by the same sender.
//!
//! Stale entries (no Approve within `APPROVAL_TTL`) are evicted.
//!
//! The queue is generic over the payload type so a single shared
//! implementation can hold either AssignHedgedJlp or WithdrawHedgedJlp
//! (M11). Each queue instance only holds one payload type — the daemon
//! runs two separate queues, one for each.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zerox1_protocol::fleet::hedgedjlp::{AssignHedgedJlp, WithdrawHedgedJlp};

use crate::resize::ResizePlan;

/// Convenience aliases — every other module references the queues by
/// these names rather than the bare generic.
pub type AssignApprovalQueue = ApprovalQueue<AssignHedgedJlp>;
pub type WithdrawApprovalQueue = ApprovalQueue<WithdrawHedgedJlp>;
/// Rebalance-resize action: queues the per-asset short-open legs the
/// rebalancer wants to add. Separate from the Assign queue so the
/// dispatch path can route the Approve to a resize-specific executor
/// (no JLP buy, only the missing hedge legs).
pub type ResizeApprovalQueue = ApprovalQueue<ResizePlan>;

/// How long an Assign sits in the pending-approval queue before eviction.
/// Default: 5 minutes — long enough for a human to inspect logs and click
/// approve, short enough that stale Assigns don't hang around indefinitely.
pub const APPROVAL_TTL: Duration = Duration::from_secs(300);

/// Maximum number of pending Assigns. Prevents unbounded memory growth.
const MAX_PENDING: usize = 64;

/// Result of an `approve()` call.
///
/// `SenderMismatch` means an Approve arrived with the right conv_id but
/// the WRONG sender pubkey — i.e. someone other than the original
/// orchestrator tried to approve. The queued entry is NOT removed in
/// this case so the legitimate sender can still approve.
pub enum ApproveResult<P> {
    Approved(P),
    NotFound,
    SenderMismatch { expected: [u8; 32], got: [u8; 32] },
}

pub struct ApprovalQueue<P> {
    // Value: (payload, enqueue time, original-sender pubkey).
    pending: Mutex<HashMap<[u8; 16], (P, Instant, [u8; 32])>>,
}

impl<P: Clone> ApprovalQueue<P> {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Add a payload to the queue, recording the original sender. Returns
    /// true if added, false if the queue was full (cap: 64 pending).
    pub fn enqueue(&self, conv: [u8; 16], payload: P, sender: [u8; 32]) -> bool {
        let mut p = self.pending.lock().unwrap();
        // Garbage-collect expired entries before inserting.
        p.retain(|_, (_, t, _)| t.elapsed() < APPROVAL_TTL);
        if p.len() >= MAX_PENDING {
            return false;
        }
        p.insert(conv, (payload, Instant::now(), sender));
        true
    }

    /// Dequeue + return the payload for `conv`, but ONLY if `sender` matches
    /// the pubkey that originally enqueued it. Returns `NotFound` if no
    /// entry exists or it has expired. Returns `SenderMismatch` if the
    /// entry exists but the sender doesn't match — the queued entry is
    /// preserved so the legitimate sender can still approve.
    pub fn approve(&self, conv: [u8; 16], sender: [u8; 32]) -> ApproveResult<P> {
        let mut p = self.pending.lock().unwrap();
        // GC first.
        p.retain(|_, (_, t, _)| t.elapsed() < APPROVAL_TTL);
        match p.get(&conv) {
            None => ApproveResult::NotFound,
            Some((_, _, original_sender)) if original_sender != &sender => {
                ApproveResult::SenderMismatch {
                    expected: *original_sender,
                    got: sender,
                }
            }
            Some(_) => {
                // Match — remove and return.
                let (payload, _, _) = p.remove(&conv).unwrap();
                ApproveResult::Approved(payload)
            }
        }
    }

    /// Non-destructive lookup — returns true if `conv` has a pending entry
    /// from `sender`. Used by dispatch to decide which queue (Assign vs
    /// Withdraw) an incoming Approve should drain.
    pub fn contains(&self, conv: [u8; 16], sender: [u8; 32]) -> bool {
        let p = self.pending.lock().unwrap();
        match p.get(&conv) {
            Some((_, t, original_sender)) => {
                t.elapsed() < APPROVAL_TTL && original_sender == &sender
            }
            None => false,
        }
    }

    #[allow(dead_code)]
    pub fn pending_count(&self) -> usize {
        let p = self.pending.lock().unwrap();
        p.values()
            .filter(|(_, t, _)| t.elapsed() < APPROVAL_TTL)
            .count()
    }
}

impl<P: Clone> Default for ApprovalQueue<P> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign() -> AssignHedgedJlp {
        AssignHedgedJlp {
            usdc_lamports: 200_000_000,
            target_delta_bps: 0,
            max_borrow_rate_bps: 3000,
            deadline_unix: 0,
        }
    }

    fn withdraw() -> WithdrawHedgedJlp {
        WithdrawHedgedJlp {
            jlp_lamports: 50_000_000,
            deadline_unix: 0,
        }
    }

    #[test]
    fn enqueue_then_approve_with_matching_sender() {
        let q: AssignApprovalQueue = ApprovalQueue::new();
        let conv = [42u8; 16];
        let sender = [7u8; 32];
        assert!(q.enqueue(conv, assign(), sender));
        match q.approve(conv, sender) {
            ApproveResult::Approved(a) => assert_eq!(a.usdc_lamports, 200_000_000),
            _ => panic!("expected Approved"),
        }
        assert_eq!(q.pending_count(), 0);
    }

    #[test]
    fn approve_with_unknown_conv_returns_not_found() {
        let q: AssignApprovalQueue = ApprovalQueue::new();
        match q.approve([99u8; 16], [0u8; 32]) {
            ApproveResult::NotFound => (),
            _ => panic!("expected NotFound"),
        }
    }

    #[test]
    fn approve_with_wrong_sender_rejects() {
        let q: AssignApprovalQueue = ApprovalQueue::new();
        let conv = [42u8; 16];
        let original = [7u8; 32];
        let attacker = [99u8; 32];
        q.enqueue(conv, assign(), original);
        match q.approve(conv, attacker) {
            ApproveResult::SenderMismatch { .. } => (),
            _ => panic!("expected SenderMismatch"),
        }
        // Critical: the queued entry must NOT have been consumed by the
        // failed approve attempt — the original sender can still approve.
        assert_eq!(q.pending_count(), 1);
        match q.approve(conv, original) {
            ApproveResult::Approved(_) => (),
            _ => panic!("expected legitimate sender to still be able to approve"),
        }
    }

    #[test]
    fn cap_prevents_unbounded_growth() {
        let q: AssignApprovalQueue = ApprovalQueue::new();
        let sender = [1u8; 32];
        for i in 0..64 {
            let mut conv = [0u8; 16];
            conv[0] = i as u8;
            assert!(q.enqueue(conv, assign(), sender));
        }
        // 65th rejected.
        let mut conv = [0u8; 16];
        conv[0] = 99;
        assert!(!q.enqueue(conv, assign(), sender));
    }

    #[test]
    fn withdraw_queue_round_trip() {
        let q: WithdrawApprovalQueue = ApprovalQueue::new();
        let conv = [33u8; 16];
        let sender = [11u8; 32];
        assert!(q.enqueue(conv, withdraw(), sender));
        match q.approve(conv, sender) {
            ApproveResult::Approved(w) => assert_eq!(w.jlp_lamports, 50_000_000),
            _ => panic!("expected Approved"),
        }
    }
}
