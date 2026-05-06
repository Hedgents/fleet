//! Manual-approval queue for AssignMultiply.
//!
//! When `require_approval=true` (default on mainnet), the dispatch
//! does NOT execute incoming Assigns immediately. Instead, it stores
//! the (conversation_id, AssignMultiply) pair, emits an
//! Escalate(Notice, NeedsApproval) envelope to the orchestrator, and
//! waits for an Approve envelope referencing the same conv_id.
//!
//! Stale entries (no Approve within `APPROVAL_TTL`) are evicted.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use zerox1_protocol::fleet::multiply::AssignMultiply;

/// How long an Assign sits in the pending-approval queue before eviction.
/// Default: 5 minutes — long enough for a human to inspect logs and click
/// approve, short enough that stale Assigns don't hang around indefinitely.
pub const APPROVAL_TTL: Duration = Duration::from_secs(300);

/// Maximum number of pending Assigns. Prevents unbounded memory growth.
const MAX_PENDING: usize = 64;

pub struct ApprovalQueue {
    pending: Mutex<HashMap<[u8; 16], (AssignMultiply, Instant)>>,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self { pending: Mutex::new(HashMap::new()) }
    }

    /// Add an Assign to the queue. Returns true if added, false if the
    /// queue was full (cap: 64 pending).
    pub fn enqueue(&self, conv: [u8; 16], assign: AssignMultiply) -> bool {
        let mut p = self.pending.lock().unwrap();
        // Garbage-collect expired entries before inserting.
        p.retain(|_, (_, t)| t.elapsed() < APPROVAL_TTL);
        if p.len() >= MAX_PENDING {
            return false;
        }
        p.insert(conv, (assign, Instant::now()));
        true
    }

    /// Remove and return the queued Assign for the given conv_id, if it
    /// exists and hasn't expired.
    pub fn approve(&self, conv: [u8; 16]) -> Option<AssignMultiply> {
        let mut p = self.pending.lock().unwrap();
        // GC first.
        p.retain(|_, (_, t)| t.elapsed() < APPROVAL_TTL);
        p.remove(&conv).map(|(a, _)| a)
    }

    #[allow(dead_code)]
    pub fn pending_count(&self) -> usize {
        let p = self.pending.lock().unwrap();
        p.values().filter(|(_, t)| t.elapsed() < APPROVAL_TTL).count()
    }
}

impl Default for ApprovalQueue {
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
    fn enqueue_then_approve() {
        let q = ApprovalQueue::new();
        let conv = [42u8; 16];
        assert!(q.enqueue(conv, assign()));
        assert_eq!(q.pending_count(), 1);
        let got = q.approve(conv);
        assert!(got.is_some());
        assert_eq!(got.unwrap().target_ltv_bps, 6000);
        assert_eq!(q.pending_count(), 0);
    }

    #[test]
    fn approve_with_unknown_conv_returns_none() {
        let q = ApprovalQueue::new();
        assert!(q.approve([99u8; 16]).is_none());
    }

    #[test]
    fn cap_prevents_unbounded_growth() {
        let q = ApprovalQueue::new();
        for i in 0..64 {
            let mut conv = [0u8; 16];
            conv[0] = i as u8;
            assert!(q.enqueue(conv, assign()));
        }
        // 65th rejected.
        let mut conv = [0u8; 16];
        conv[0] = 99;
        assert!(!q.enqueue(conv, assign()));
    }
}
