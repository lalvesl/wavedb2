//! [`Directory`] page I/O — routing records into bucket pages over a
//! [`BlockFile`], the half of the directory that touches disk.
//!
//! The addressing math and the container live in [`crate::directory`]; this
//! module reads, rewrites, splits, and compresses the pages the slots point
//! at, and persists the type's [`Dictionary`] alongside them.

use wavedb_core::Id;

use crate::block::{BLOCK_SIZE, BlockAllocator, BlockDescriptor};
use crate::block_file::BlockFile;
use crate::dictionary::Dictionary;
use crate::directory::Directory;
use crate::error::StorageResult;
use crate::page::SlotPage;

impl Directory {
    /// Read the [`SlotPage`] backing `bucket`, or a fresh empty page if the slot
    /// is unallocated.
    ///
    /// # Errors
    /// [`StorageError::Io`](crate::StorageError::Io) on a read fault or
    /// [`StorageError::Corrupt`](crate::StorageError::Corrupt) if the page fails
    /// its crc / bounds checks.
    pub fn read_page(
        &self,
        struct_hash: u64,
        file: &BlockFile,
        bucket: usize,
    ) -> StorageResult<SlotPage> {
        let desc = self.slots[bucket];
        if !desc.is_allocated() {
            return Ok(SlotPage::new(struct_hash));
        }
        let page =
            SlotPage::from_bytes(&file.read_run(desc.run())?, &self.dict)?;
        debug_assert_eq!(page.struct_hash(), struct_hash);
        Ok(page)
    }

    /// The record bytes stored at `id`, if present.
    ///
    /// # Errors
    /// Propagates read / corruption faults from [`read_page`](Self::read_page).
    pub fn get_record(
        &self,
        struct_hash: u64,
        file: &BlockFile,
        id: Id,
    ) -> StorageResult<Option<Vec<u8>>> {
        let bucket = self.bucket_of(id.raw());
        Ok(self
            .read_page(struct_hash, file, bucket)?
            .get(id)
            .map(<[u8]>::to_vec))
    }

    /// Route `id` to its bucket, upsert its bytes, and rewrite the page.
    ///
    /// Crash-safe ordering: the new page is allocated and written **before** the
    /// directory slot is repointed, and the old run is freed only afterwards — so
    /// the slot never names a half-written page.
    ///
    /// # Errors
    /// Propagates read / write / corruption faults.
    pub fn upsert_record(
        &mut self,
        struct_hash: u64,
        file: &BlockFile,
        alloc: &mut BlockAllocator,
        id: Id,
        bytes: Vec<u8>,
    ) -> StorageResult<()> {
        let bucket = self.bucket_of(id.raw());
        let old = self.slots[bucket];
        let mut page = self.read_page(struct_hash, file, bucket)?;
        if self.compress {
            // Warm the dictionary with the settling record (append-only,
            // capped); the page written below binds the grown state, and the
            // grown buffer is re-persisted to its own run.
            let before = self.dict.len();
            self.dict.sample(&bytes);
            if self.dict.len() != before {
                self.persist_dict(file, alloc)?;
            }
        }
        page.upsert(id, bytes);
        self.slots[bucket] =
            place(file, alloc, &page, &self.dict, self.compress)?;
        if old.is_allocated() {
            alloc.free(old.run());
        }
        Ok(())
    }

    /// Rewrite the dictionary's block run after growth: allocate + write the
    /// new run, repoint, then free the old one — the same crash-safe ordering
    /// pages use.
    fn persist_dict(
        &mut self,
        file: &BlockFile,
        alloc: &mut BlockAllocator,
    ) -> StorageResult<()> {
        let bytes = self.dict.to_bytes();
        let run = alloc.alloc(bytes.len().div_ceil(BLOCK_SIZE) as u64);
        file.write_run(run, &bytes)?;
        let old = self.dict_desc;
        self.dict_desc = BlockDescriptor::from_run(
            run,
            occupation_of(bytes.len() as u64, run.byte_len()),
        );
        if old.is_allocated() {
            alloc.free(old.run());
        }
        Ok(())
    }

    /// Remove `id` from its bucket page. Returns whether it was present.
    ///
    /// # Errors
    /// Propagates read / write / corruption faults.
    pub fn remove_record(
        &mut self,
        struct_hash: u64,
        file: &BlockFile,
        alloc: &mut BlockAllocator,
        id: Id,
    ) -> StorageResult<bool> {
        let bucket = self.bucket_of(id.raw());
        let old = self.slots[bucket];
        if !old.is_allocated() {
            return Ok(false);
        }
        let mut page = self.read_page(struct_hash, file, bucket)?;
        let existed = page.remove(id).is_some();
        self.slots[bucket] =
            place(file, alloc, &page, &self.dict, self.compress)?;
        alloc.free(old.run());
        Ok(existed)
    }

    /// Split the next bucket in round-robin order, repartitioning its records by
    /// the next hash bit and appending one new bucket — the page-moving half of
    /// linear-hashing growth.
    ///
    /// # Errors
    /// Propagates read / write / corruption faults.
    pub fn split_next(
        &mut self,
        struct_hash: u64,
        file: &BlockFile,
        alloc: &mut BlockAllocator,
    ) -> StorageResult<()> {
        let level = self.split_bit();
        let s = self.next_split_bucket() as usize;
        let old = self.slots[s];

        let mut keep = SlotPage::new(struct_hash);
        let mut moved = SlotPage::new(struct_hash);
        for (id, bytes) in self.read_page(struct_hash, file, s)?.into_entries()
        {
            // Bit `level` decides: 0 stays in `s`, 1 moves to the new bucket.
            if (self.hash(id.raw()) >> level) & 1 == 0 {
                keep.upsert(id, bytes);
            } else {
                moved.upsert(id, bytes);
            }
        }

        let keep_desc = place(file, alloc, &keep, &self.dict, self.compress)?;
        let moved_desc = place(file, alloc, &moved, &self.dict, self.compress)?;
        self.slots[s] = keep_desc;
        self.slots.push(moved_desc); // new bucket at index M (= s + base)
        if old.is_allocated() {
            alloc.free(old.run());
        }
        Ok(())
    }
}

/// Allocate a run sized to `page`, write it, and return its descriptor. An empty
/// page allocates nothing and yields [`BlockDescriptor::EMPTY`].
fn place(
    file: &BlockFile,
    alloc: &mut BlockAllocator,
    page: &SlotPage,
    dict: &Dictionary,
    compress: bool,
) -> StorageResult<BlockDescriptor> {
    if page.is_empty() {
        return Ok(BlockDescriptor::EMPTY);
    }
    let bytes = page.to_bytes(dict, compress)?;
    let run = alloc.alloc(bytes.len().div_ceil(BLOCK_SIZE) as u64);
    file.write_run(run, &bytes)?;
    Ok(BlockDescriptor::from_run(
        run,
        occupation_of(bytes.len() as u64, run.byte_len()),
    ))
}

/// Coarse 1/16th fill gauge: how full a page's bytes leave its allocated run.
fn occupation_of(used: u64, capacity: u64) -> u8 {
    if capacity == 0 {
        return 0;
    }
    (used.saturating_mul(16) / capacity).min(15) as u8
}

#[cfg(test)]
mod tests {
    use crate::block::BlockAllocator;
    use crate::block_file::{BlockFile, RESERVED_BLOCKS};
    use crate::dictionary::Dictionary;
    use crate::directory::Directory;
    use wavedb_core::{Id, U48};

    /// A temp `data.bin` plus an allocator with block 0 (the superblock) reserved.
    fn backed() -> (tempfile::TempDir, BlockFile, BlockAllocator) {
        let dir = tempfile::tempdir().unwrap();
        let bf = BlockFile::open(dir.path().join("data.bin")).unwrap();
        let mut alloc = BlockAllocator::new();
        alloc.alloc(RESERVED_BLOCKS); // never hand out the superblock
        (dir, bf, alloc)
    }

    fn rec_id(key: u64) -> Id {
        Id::new(key, U48::from(1u32), false, (key & 0x7FFF) as u16)
    }

    // The dictionary lives in `data.bin` too: sampling growth allocates and
    // repoints its own block run; a compression-off directory never samples,
    // never allocates. Either way the pages themselves stay readable, and the
    // persisted run round-trips byte-identically.
    #[test]
    fn dictionary_is_persisted_in_its_own_run() {
        let (_d, bf, mut alloc) = backed();

        let mut on = Directory::new(bf.seed());
        assert!(!on.dict_descriptor().is_allocated());
        on.upsert_record(1, &bf, &mut alloc, rec_id(1), vec![0xAB; 100])
            .unwrap();
        let desc = on.dict_descriptor();
        assert!(desc.is_allocated(), "sampling must persist the dictionary");
        let stored =
            Dictionary::from_bytes(&bf.read_run(desc.run()).unwrap()).unwrap();
        assert_eq!(stored.latest(), &[0xAB; 100][..]);
        assert_eq!(
            on.get_record(1, &bf, rec_id(1)).unwrap(),
            Some(vec![0xAB; 100])
        );

        let mut off = Directory::new(bf.seed()).with_compression(false);
        off.upsert_record(2, &bf, &mut alloc, rec_id(1), vec![0xCD; 100])
            .unwrap();
        assert!(
            !off.dict_descriptor().is_allocated(),
            "compression off ⇒ no dictionary run"
        );
        assert_eq!(
            off.get_record(2, &bf, rec_id(1)).unwrap(),
            Some(vec![0xCD; 100])
        );
    }

    #[test]
    fn upsert_get_remove_through_pages() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        let sh = 0xABCD;

        dir.upsert_record(sh, &bf, &mut alloc, rec_id(1), vec![1, 2, 3])
            .unwrap();
        dir.upsert_record(sh, &bf, &mut alloc, rec_id(2), vec![4, 5])
            .unwrap();
        assert_eq!(
            dir.get_record(sh, &bf, rec_id(1)).unwrap(),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            dir.get_record(sh, &bf, rec_id(2)).unwrap(),
            Some(vec![4, 5])
        );
        assert_eq!(dir.get_record(sh, &bf, rec_id(9)).unwrap(), None);

        // Overwrite, then remove.
        dir.upsert_record(sh, &bf, &mut alloc, rec_id(1), vec![9])
            .unwrap();
        assert_eq!(dir.get_record(sh, &bf, rec_id(1)).unwrap(), Some(vec![9]));
        assert!(dir.remove_record(sh, &bf, &mut alloc, rec_id(1)).unwrap());
        assert_eq!(dir.get_record(sh, &bf, rec_id(1)).unwrap(), None);
        assert!(!dir.remove_record(sh, &bf, &mut alloc, rec_id(1)).unwrap());
    }

    #[test]
    fn split_preserves_all_records() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        let sh = 1;

        for k in 0..200u64 {
            dir.upsert_record(sh, &bf, &mut alloc, rec_id(k), vec![k as u8; 4])
                .unwrap();
        }
        assert_eq!(dir.len(), 1);

        dir.split_next(sh, &bf, &mut alloc).unwrap();
        assert_eq!(dir.len(), 2);

        // Every record still resolves after the repartition.
        for k in 0..200u64 {
            assert_eq!(
                dir.get_record(sh, &bf, rec_id(k)).unwrap(),
                Some(vec![k as u8; 4]),
                "record {k} lost across split"
            );
        }
    }

    #[test]
    fn split_empty_bucket_is_noop_growth() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        dir.split_next(1, &bf, &mut alloc).unwrap();
        assert_eq!(dir.len(), 2);
    }
}
