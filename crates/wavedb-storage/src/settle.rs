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
                let bytes = slot.get(*id);
                match bytes {
                    Some(b) => {
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
                    }
                    None => {
                        dir.remove_record(
                            slot.struct_hash(),
                            &self.file,
                            &mut alloc,
                            *id,
                            &dict,
                        )?;
                    }
                }
            }
        }
        Ok(())
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
