//! The settle path — converging touched ids' pages to the caches' current
//! state, split from [`crate::page_store`] for the file budget. Runs inline
//! with `apply` today; the background drain task (M2 tail) lands here.

use wavedb_core::Id;

use crate::block::BlockAllocator;
use crate::block_file::BlockFile;
use crate::dictionary::DictState;
use crate::directory::Directory;
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
                merge_rounds(&mut self.pending.lock(), round);
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

    /// Converge the touched ids' pages to the caches' current state: present
    /// in cache ⇒ upsert those bytes, absent ⇒ remove. Writing cache state
    /// (not batch state) makes settling idempotent and order-independent.
    // The directory + dictionary guards must span one slot's whole settle
    // loop — both are read and rewritten across every id; the lint's "merge
    // into single usage" would drop them mid-mutation.
    #[allow(clippy::significant_drop_tightening)]
    pub(crate) fn settle(&self, touched: &Touched) -> StorageResult<()> {
        for (idx, ids) in touched {
            let slot = self.types[*idx];
            let mut dir_guard = slot.directory().lock();
            let dir =
                dir_guard.get_or_insert_with(|| Directory::new(self.seed));
            let mut dict = slot.dictionary().lock();
            let mut alloc = self.alloc.lock();
            for id in ids {
                if let Some(b) = slot.get(*id) {
                    let sh = slot.struct_hash();
                    dir.upsert_record(
                        sh, &self.file, &mut alloc, *id, b, &mut dict,
                    )?;
                    maybe_split(
                        dir,
                        sh,
                        &self.file,
                        &mut alloc,
                        self.split_threshold_blocks,
                        *id,
                        &dict,
                    )?;
                } else {
                    dir.remove_record(
                        slot.struct_hash(),
                        &self.file,
                        &mut alloc,
                        *id,
                        &dict,
                    )?;
                    // The page now agrees the id is gone.
                    slot.clear_removed(*id);
                }
            }
        }
        Ok(())
    }
}

/// Merge a failed round back into the pending queue (slot-grouped; ids may
/// duplicate what landed meanwhile — settling twice is idempotent).
fn merge_rounds(pending: &mut Touched, round: Touched) {
    for (idx, ids) in round {
        match pending.iter_mut().find(|(i, _)| *i == idx) {
            Some((_, existing)) => existing.extend(ids),
            None => pending.push((idx, ids)),
        }
    }
}

/// Split the directory's next round-robin bucket when the page the write landed
/// in has grown past the threshold, keeping page sizes bounded.
///
/// Only a `Put` grows a page, so checking just the touched bucket (O(1), not a
/// scan of every slot) still catches every over-threshold page at the moment it
/// crosses the line. Linear hashing splits round-robin, so the split may relieve
/// a different bucket first — repeated puts into a fat bucket keep triggering
/// splits until the round-robin pointer reaches it (same behaviour the full scan
/// had).
fn maybe_split(
    dir: &mut Directory,
    struct_hash: u64,
    file: &BlockFile,
    alloc: &mut BlockAllocator,
    threshold: u64,
    id: Id,
    dict: &DictState,
) -> StorageResult<()> {
    let touched = dir.bucket_of(id.raw());
    if dir.descriptor(touched).count() > threshold {
        dir.split_next(struct_hash, file, alloc, dict)?;
    }
    Ok(())
}
