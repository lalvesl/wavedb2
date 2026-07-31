//! [`PageStore`]'s commit path — the [`Store`] impl and the batch pipeline
//! behind it: route, validate guards, journal (the durability point),
//! commit to the per-type caches, queue the settle.
//!
//! Split from [`page_store`](crate::page_store) (which keeps open/recovery
//! and the accessors) the same way the read fallback lives in
//! `read_through`.

use wavedb_core::{Id, Result as CoreResult, Store, Write};

use crate::error::{StorageError, StorageResult};
use crate::page_store::{PageStore, Touched};

impl PageStore {
    /// Journal first (durable fsync), then commit to the per-type caches and
    /// queue the touched ids for the settle drain. The batch is durable when
    /// this returns; the page write happens later
    /// ([`drain`](Self::drain)) — a crash before it replays from the journal.
    // The journal guard deliberately spans the cache commit — releasing it
    // earlier (the lint's suggestion) would let two applies commit their
    // caches in the opposite order to their journal frames, so replay could
    // disagree with what live readers saw. The pending-queue push also stays
    // under it: eviction relies on "empty queue under its lock ⇒ every
    // committed id is settled" (see `evict_settled`).
    #[allow(clippy::significant_drop_tightening)]
    fn apply_inner(&self, batch: &[Write]) -> StorageResult<()> {
        // Refuse unroutable writes *before* the journal sees the batch, so the
        // log never holds a batch replay would choke on.
        self.route_batch(batch)?;
        let mut journal = self.journal.lock();
        // Guards validate against the pre-batch state under the same lock
        // that orders commits; a mismatch refuses the batch before the
        // durability point. Guards are not state — the journal never holds
        // them, so replay re-applies only the effective writes.
        for w in batch {
            if let Write::Expect(id, expected) = w
                && self.read_any(*id)? != *expected
            {
                return Err(StorageError::Core(wavedb_core::Error::Conflict(
                    *id,
                )));
            }
        }
        let effective: Vec<Write> = batch
            .iter()
            .filter(|w| !matches!(w, Write::Expect(..)))
            .cloned()
            .collect();
        journal
            .append(&crate::journal::JournalFrame::Batch(effective.clone()))?; // durability point
        // Commit under the journal lock: cache order == journal order.
        let touched = self.commit_to_caches(&effective)?;
        merge_touched(&mut self.pending.lock(), touched);
        Ok(())
    }

    /// Read `id`'s current bytes without knowing its type: caches first,
    /// then settled pages — the sync body [`Store::get`] wraps, reused by
    /// the `Expect` guard (whose absent-expectation carries no type head to
    /// route by).
    fn read_any(&self, id: Id) -> StorageResult<Option<Vec<u8>>> {
        if let Some(bytes) = self.types.iter().find_map(|s| s.get(id)) {
            return Ok(Some(bytes));
        }
        for slot in &self.types {
            if let Some(bytes) = self.read_from_pages(slot, id)? {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    /// Verify every `Put` in `batch` routes to a registered slot.
    pub(crate) fn route_batch(&self, batch: &[Write]) -> StorageResult<()> {
        for w in batch {
            if let Write::Put(_, bytes) = w {
                let sh = struct_hash_of(bytes)?;
                if self.slot_of(sh).is_none() {
                    return Err(StorageError::UnregisteredStructHash(sh));
                }
            }
        }
        Ok(())
    }

    /// Apply `batch` to the per-type caches (routing validated beforehand),
    /// returning the touched ids per slot. Runs under the journal lock, so
    /// commits are ordered and each type's write guard is held only briefly.
    ///
    /// A page probe (routing a `Remove` whose owner is not cached) can fail
    /// on a disk fault — *after* the durability point. The live state then
    /// under-applies the batch, but the journal holds it whole: a reopen
    /// replays it correctly, which is the strongest promise a broken disk
    /// leaves available.
    pub(crate) fn commit_to_caches(
        &self,
        batch: &[Write],
    ) -> StorageResult<Touched> {
        let mut touched: Touched = Vec::new();
        for w in batch {
            match w {
                Write::Put(id, bytes) => {
                    let sh = struct_hash_of(bytes)
                        .expect("route_batch validated the head");
                    let idx = self
                        .types
                        .binary_search_by_key(&sh, |s| s.struct_hash())
                        .expect("route_batch validated registration");
                    self.types[idx]
                        .mem_cache()
                        .write()
                        .insert(id.raw(), bytes.clone());
                    // A Put supersedes any unsettled remove of the same id.
                    self.types[idx].clear_removed(*id);
                    note_touched(&mut touched, idx, *id);
                }
                Write::Remove(id) => {
                    if let Some(idx) = self.owner_of(*id)? {
                        self.types[idx].mem_cache().write().remove(&id.raw());
                        // The page still holds the bytes until the settle
                        // lands — tombstone so reads don't resurrect them.
                        self.types[idx].mark_removed(*id);
                        note_touched(&mut touched, idx, *id);
                    }
                }
                // Validated and stripped in `apply_inner`; never reaches a
                // journaled batch (nor, therefore, replay).
                Write::Expect(..) => {}
            }
        }
        Ok(touched)
    }
}

impl Store for PageStore {
    async fn get(&self, id: Id) -> CoreResult<Option<Vec<u8>>> {
        // Untyped fallback: the id alone names no type, so probe every slot —
        // caches first (cheap), then settled pages. Typed callers use
        // `get_of` and skip this scan entirely.
        Ok(self.read_any(id)?)
    }

    async fn get_of(
        &self,
        struct_hash: u64,
        id: Id,
    ) -> CoreResult<Option<Vec<u8>>> {
        // One binary search on the route table, then this type's own cache
        // read lock — a cached read of one type never contends with another
        // type's. A miss reads through to the settled page.
        let Some(slot) = self.slot_of(struct_hash) else {
            return Ok(None);
        };
        if let Some(bytes) = slot.get(id) {
            return Ok(Some(bytes));
        }
        Ok(self.read_from_pages(slot, id)?)
    }

    async fn apply(&self, batch: &[Write]) -> CoreResult<()> {
        self.apply_inner(batch)?; // StorageError → core::Error (typed seam)
        Ok(())
    }
}

/// Record `id` as touched under slot `idx`.
fn note_touched(touched: &mut Touched, idx: usize, id: Id) {
    match touched.iter_mut().find(|(i, _)| *i == idx) {
        Some((_, ids)) => ids.push(id),
        None => touched.push((idx, vec![id])),
    }
}

/// Merge one batch's touched ids into the pending queue (slot-grouped).
fn merge_touched(pending: &mut Touched, touched: Touched) {
    for (idx, ids) in touched {
        for id in ids {
            note_touched(pending, idx, id);
        }
    }
}

/// The `STRUCT_HASH` at the head of a wire-encoded record (`[STRUCT_HASH][…]`).
fn struct_hash_of(bytes: &[u8]) -> StorageResult<u64> {
    if bytes.len() < 8 {
        return Err(StorageError::Corrupt("record shorter than STRUCT_HASH"));
    }
    Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}
