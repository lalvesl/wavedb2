//! The read-through half of the cache-or-page read path (split from
//! [`crate::page_store`] for the file budget): serving an id the cache no
//! longer holds from its settled page, and routing a `Remove` whose owner
//! is not cached.

use wavedb_core::Id;

use crate::error::StorageResult;
use crate::page_store::PageStore;
use crate::struct_storage::StructStorage;

impl PageStore {
    /// The slot index owning `id`, if any. The id alone names no type: the
    /// owner is whichever cache holds it, falling back to the settled pages
    /// for a record the cache no longer carries. Writers are serialised by
    /// the journal lock, so the probe-then-mutate pair can't race.
    pub(crate) fn owner_of(&self, id: Id) -> StorageResult<Option<usize>> {
        let cached = self
            .types
            .iter()
            .position(|s| s.mem_cache().read().contains_key(&id.raw()));
        if let Some(idx) = cached {
            return Ok(Some(idx));
        }
        for (idx, slot) in self.types.iter().enumerate() {
            if self.read_from_pages(slot, id)?.is_some() {
                return Ok(Some(idx));
            }
        }
        Ok(None)
    }

    /// Read `id` from `slot`'s settled pages — the fallback when the cache
    /// does not hold it. `None` when the type has never settled anything,
    /// the page does not hold the id, or an unsettled remove tombstones it
    /// (the page's bytes are stale until the settle lands).
    // The directory guard must span the page read — `dir` borrows from it.
    #[allow(clippy::significant_drop_tightening)]
    pub(crate) fn read_from_pages(
        &self,
        slot: &'static StructStorage,
        id: Id,
    ) -> StorageResult<Option<Vec<u8>>> {
        if slot.is_removed(id) {
            return Ok(None);
        }
        let dir_guard = slot.directory().lock();
        let Some(dir) = dir_guard.as_ref() else {
            return Ok(None);
        };
        let dict = slot.dictionary().lock();
        dir.get_record(slot.struct_hash(), &self.file, id, &dict)
    }
}
