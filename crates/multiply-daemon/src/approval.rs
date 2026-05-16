//! Manual-approval queue for AssignMultiply.
//!
//! When `require_approval=true` (default on mainnet), the dispatch
//! does NOT execute incoming Assigns immediately. Instead, it stores
//! the (conversation_id, AssignMultiply, sender_pubkey) tuple, emits
//! an Escalate(Notice, NeedsApproval) envelope to the orchestrator,
//! and waits for an Approve envelope referencing the same conv_id
//! AND signed by the same sender.
//!
//! Stale entries (no Approve within `APPROVAL_TTL`) are evicted.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zerox1_protocol::fleet::multiply::{AssignMultiply, WithdrawMultiply};

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
pub enum ApproveResult {
    Approved(AssignMultiply),
    NotFound,
    SenderMismatch { expected: [u8; 32], got: [u8; 32] },
}

pub struct ApprovalQueue {
    // Value: (assign payload, enqueue time, original-sender pubkey).
    pending: Mutex<HashMap<[u8; 16], (AssignMultiply, Instant, [u8; 32])>>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Add an Assign to the queue, recording the original sender. Returns
    /// true if added, false if the queue was full (cap: 64 pending).
    pub fn enqueue(&self, conv: [u8; 16], assign: AssignMultiply, sender: [u8; 32]) -> bool {
        let mut p = self.pending.lock().unwrap();
        // Garbage-collect expired entries before inserting.
        p.retain(|_, (_, t, _)| t.elapsed() < APPROVAL_TTL);
        if p.len() >= MAX_PENDING {
            return false;
        }
        p.insert(conv, (assign, Instant::now(), sender));
        true
    }

    /// Dequeue + return the Assign for `conv`, but ONLY if `sender` matches
    /// the pubkey that originally enqueued it. Returns `NotFound` if no
    /// entry exists or it has expired. Returns `SenderMismatch` if the
    /// entry exists but the sender doesn't match — the queued entry is
    /// preserved so the legitimate sender can still approve.
    pub fn approve(&self, conv: [u8; 16], sender: [u8; 32]) -> ApproveResult {
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
                let (assign, _, _) = p.remove(&conv).unwrap();
                ApproveResult::Approved(assign)
            }
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

impl Default for ApprovalQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Parallel approval queue for `WithdrawMultiply` payloads. Same shape as
/// [`ApprovalQueue`] but keyed on the withdraw payload type. We keep it as
/// a distinct type (vs. a tagged enum) so the dispatch loop's Approve arm
/// can probe both queues independently with type-safe payloads — and so
/// that conv_id collisions between Assign and Withdraw flows on the same
/// orchestrator are isolated.
pub enum WithdrawApproveResult {
    Approved(WithdrawMultiply),
    NotFound,
    SenderMismatch { expected: [u8; 32], got: [u8; 32] },
}

pub struct WithdrawApprovalQueue {
    pending: Mutex<HashMap<[u8; 16], (WithdrawMultiply, Instant, [u8; 32])>>,
}

impl WithdrawApprovalQueue {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Enqueue a `WithdrawMultiply`. Returns true on success, false if
    /// the queue is at capacity (same `MAX_PENDING` as the Assign queue).
    pub fn enqueue(&self, conv: [u8; 16], withdraw: WithdrawMultiply, sender: [u8; 32]) -> bool {
        let mut p = self.pending.lock().unwrap();
        p.retain(|_, (_, t, _)| t.elapsed() < APPROVAL_TTL);
        if p.len() >= MAX_PENDING {
            return false;
        }
        p.insert(conv, (withdraw, Instant::now(), sender));
        true
    }

    /// Approve + dequeue, with the same sender-match safety as
    /// [`ApprovalQueue::approve`].
    pub fn approve(&self, conv: [u8; 16], sender: [u8; 32]) -> WithdrawApproveResult {
        let mut p = self.pending.lock().unwrap();
        p.retain(|_, (_, t, _)| t.elapsed() < APPROVAL_TTL);
        match p.get(&conv) {
            None => WithdrawApproveResult::NotFound,
            Some((_, _, original_sender)) if original_sender != &sender => {
                WithdrawApproveResult::SenderMismatch {
                    expected: *original_sender,
                    got: sender,
                }
            }
            Some(_) => {
                let (w, _, _) = p.remove(&conv).unwrap();
                WithdrawApproveResult::Approved(w)
            }
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

impl Default for WithdrawApprovalQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assign() -> AssignMultiply {
        AssignMultiply {
            vault: [0; 32],
            target_ltv_bps: 6000,
            max_slippage_bps: 50,
            deadline_unix: 0,
        }
    }

    #[test]
    fn enqueue_then_approve_with_matching_sender() {
        let q = ApprovalQueue::new();
        let conv = [42u8; 16];
        let sender = [7u8; 32];
        assert!(q.enqueue(conv, assign(), sender));
        match q.approve(conv, sender) {
            ApproveResult::Approved(a) => assert_eq!(a.target_ltv_bps, 6000),
            _ => panic!("expected Approved"),
        }
        assert_eq!(q.pending_count(), 0);
    }

    #[test]
    fn approve_with_unknown_conv_returns_not_found() {
        let q = ApprovalQueue::new();
        match q.approve([99u8; 16], [0u8; 32]) {
            ApproveResult::NotFound => (),
            _ => panic!("expected NotFound"),
        }
    }

    #[test]
    fn approve_with_wrong_sender_rejects() {
        let q = ApprovalQueue::new();
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
        let q = ApprovalQueue::new();
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

    fn withdraw() -> WithdrawMultiply {
        WithdrawMultiply {
            vault: [3u8; 32],
            max_slippage_bps: 100,
            deadline_unix: 0,
        }
    }

    #[test]
    fn withdraw_queue_enqueue_then_approve_with_matching_sender() {
        let q = WithdrawApprovalQueue::new();
        let conv = [42u8; 16];
        let sender = [7u8; 32];
        assert!(q.enqueue(conv, withdraw(), sender));
        match q.approve(conv, sender) {
            WithdrawApproveResult::Approved(w) => assert_eq!(w.max_slippage_bps, 100),
            _ => panic!("expected Approved"),
        }
        assert_eq!(q.pending_count(), 0);
    }

    #[test]
    fn withdraw_queue_unknown_conv_returns_not_found() {
        let q = WithdrawApprovalQueue::new();
        match q.approve([99u8; 16], [0u8; 32]) {
            WithdrawApproveResult::NotFound => (),
            _ => panic!("expected NotFound"),
        }
    }

    #[test]
    fn withdraw_queue_wrong_sender_preserves_entry() {
        let q = WithdrawApprovalQueue::new();
        let conv = [42u8; 16];
        let original = [7u8; 32];
        let attacker = [99u8; 32];
        q.enqueue(conv, withdraw(), original);
        match q.approve(conv, attacker) {
            WithdrawApproveResult::SenderMismatch { .. } => (),
            _ => panic!("expected SenderMismatch"),
        }
        assert_eq!(q.pending_count(), 1);
        match q.approve(conv, original) {
            WithdrawApproveResult::Approved(_) => (),
            _ => panic!("legitimate sender should still be able to approve"),
        }
    }
}
