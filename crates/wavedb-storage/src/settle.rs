//! The settle drain — feeding rounds of touched ids to the checkpoint writer,
//! plus the cache-eviction policy.
//!
//! The work itself lives in [`crate::plan`] (page images, grouped per bucket)
//! and [`crate::checkpoint`] (one window, one write, then the descriptor
//! swap). This module owns only the queue side: what a round is, when it
//! retries, and when a settled entry may leave the cache.

use crate::error::StorageResult;
use crate::page_store::{PageStore, Touched};

impl PageStore {
    /// Settle everything queued: drain the pending touched ids into their
    /// pages. Loops until the queue is observed empty (writes landing while
    /// a round settles are picked up by the next round).
    ///
    /// # Errors
    /// A page write fault. Nothing acked is at risk — the journal still
    /// holds every unsettled batch; the failed round's ids stay pending so
    /// a later drain (or the reopen replay) retries them.
    pub fn drain(&self) -> StorageResult<()> {
        loop {
            let round = std::mem::take(&mut *self.pending.lock());
            if round.is_empty() {
                return Ok(());
            }
            if let Err(e) = self.settle(&round) {
                // Put the round back: ids may be partially settled, but
                // settle writes cache state, so re-settling is idempotent.
                requeue(&mut self.pending.lock(), round);
                return Err(e);
            }
        }
    }

    /// Whether any committed batch still awaits its page write.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    /// Bytes of committed journal frames — the checkpoint policy's input.
    #[must_use]
    pub fn journal_len(&self) -> u64 {
        self.journal.lock().len_bytes()
    }

    /// Evict settled cache entries until the caches hold at most
    /// `budget_bytes`. A no-op while anything is pending — only a settled
    /// entry may leave the cache (the page then serves reads). Quiesces
    /// writers for the duration (journal lock), so "queue empty" can't race
    /// a commit whose ids aren't queued yet.
    #[allow(clippy::significant_drop_tightening)]
    pub fn evict_settled(&self, budget_bytes: usize) {
        let _journal = self.journal.lock();
        let pending = self.pending.lock();
        if !pending.is_empty() {
            return;
        }
        let mut total: usize =
            self.types.iter().map(|s| s.cached_bytes()).sum();
        for slot in &self.types {
            if total <= budget_bytes {
                return;
            }
            total -= slot.evict_up_to(total - budget_bytes);
        }
    }
}

/// Merge a failed round back into the pending queue (slot-grouped; ids may
/// duplicate what landed meanwhile — settling twice is idempotent).
pub fn requeue(pending: &mut Touched, round: Touched) {
    for (idx, ids) in round {
        match pending.iter_mut().find(|(i, _)| *i == idx) {
            Some((_, existing)) => existing.extend(ids),
            None => pending.push((idx, ids)),
        }
    }
}
