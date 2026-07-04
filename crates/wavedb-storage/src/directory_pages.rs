//! [`Directory`] page I/O — routing records into bucket pages over a
//! [`BlockFile`], the half of the directory that touches disk.
//!
//! The addressing math and the container live in [`crate::directory`]; this
//! module reads, rewrites, splits, and compresses the pages the slots point
//! at. The type's compression state ([`DictState`]) is not directory state —
//! it lives in the type's `StructStorage` slot and is passed in per call.

use wavedb_core::Id;

use crate::block::{BLOCK_SIZE, BlockAllocator, BlockDescriptor};
use crate::block_file::BlockFile;
use crate::dictionary::DictState;
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
        dict: &DictState,
    ) -> StorageResult<SlotPage> {
        let desc = self.slots[bucket];
        if !desc.is_allocated() {
            return Ok(SlotPage::new(struct_hash));
        }
        let page = SlotPage::from_bytes(
            &file.read_run(desc.run())?,
            dict.dictionary(),
        )?;
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
        dict: &DictState,
    ) -> StorageResult<Option<Vec<u8>>> {
        let bucket = self.bucket_of(id.raw());
        Ok(self
            .read_page(struct_hash, file, bucket, dict)?
            .get(id)
            .map(<[u8]>::to_vec))
    }

    /// Route `id` to its bucket, upsert its bytes, and rewrite the page —
    /// warming the type's dictionary with the settling record first, so the
    /// page written below binds the grown state.
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
        dict: &mut DictState,
    ) -> StorageResult<()> {
        let bucket = self.bucket_of(id.raw());
        let old = self.slots[bucket];
        let mut page = self.read_page(struct_hash, file, bucket, dict)?;
        dict.warm(&bytes, file, alloc)?;
        page.upsert(id, bytes);
        self.slots[bucket] = place(file, alloc, &page, dict)?;
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
        dict: &DictState,
    ) -> StorageResult<bool> {
        let bucket = self.bucket_of(id.raw());
        let old = self.slots[bucket];
        if !old.is_allocated() {
            return Ok(false);
        }
        let mut page = self.read_page(struct_hash, file, bucket, dict)?;
        let existed = page.remove(id).is_some();
        self.slots[bucket] = place(file, alloc, &page, dict)?;
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
        dict: &DictState,
    ) -> StorageResult<()> {
        let level = self.split_bit();
        let s = self.next_split_bucket() as usize;
        let old = self.slots[s];

        let mut keep = SlotPage::new(struct_hash);
        let mut moved = SlotPage::new(struct_hash);
        for (id, bytes) in
            self.read_page(struct_hash, file, s, dict)?.into_entries()
        {
            // Bit `level` decides: 0 stays in `s`, 1 moves to the new bucket.
            if (self.hash(id.raw()) >> level) & 1 == 0 {
                keep.upsert(id, bytes);
            } else {
                moved.upsert(id, bytes);
            }
        }

        let keep_desc = place(file, alloc, &keep, dict)?;
        let moved_desc = place(file, alloc, &moved, dict)?;
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
    dict: &DictState,
) -> StorageResult<BlockDescriptor> {
    if page.is_empty() {
        return Ok(BlockDescriptor::EMPTY);
    }
    let bytes = page.to_bytes(dict.dictionary(), dict.enabled())?;
    let run = alloc.alloc(bytes.len().div_ceil(BLOCK_SIZE) as u64);
    file.write_run(run, &bytes)?;
    Ok(BlockDescriptor::from_run_used(run, bytes.len() as u64))
}

#[cfg(test)]
mod tests {
    use crate::block::BlockAllocator;
    use crate::block_file::{BlockFile, RESERVED_BLOCKS};
    use crate::dictionary::DictState;
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

    #[test]
    fn upsert_get_remove_through_pages() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        let mut dict = DictState::new(true);
        let sh = 0xABCD;

        dir.upsert_record(
            sh,
            &bf,
            &mut alloc,
            rec_id(1),
            vec![1, 2, 3],
            &mut dict,
        )
        .unwrap();
        dir.upsert_record(
            sh,
            &bf,
            &mut alloc,
            rec_id(2),
            vec![4, 5],
            &mut dict,
        )
        .unwrap();
        assert!(
            dict.descriptor().is_allocated(),
            "sampling must persist the dictionary"
        );
        assert_eq!(
            dir.get_record(sh, &bf, rec_id(1), &dict).unwrap(),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            dir.get_record(sh, &bf, rec_id(2), &dict).unwrap(),
            Some(vec![4, 5])
        );
        assert_eq!(dir.get_record(sh, &bf, rec_id(9), &dict).unwrap(), None);

        // Overwrite, then remove.
        dir.upsert_record(sh, &bf, &mut alloc, rec_id(1), vec![9], &mut dict)
            .unwrap();
        assert_eq!(
            dir.get_record(sh, &bf, rec_id(1), &dict).unwrap(),
            Some(vec![9])
        );
        assert!(
            dir.remove_record(sh, &bf, &mut alloc, rec_id(1), &dict)
                .unwrap()
        );
        assert_eq!(dir.get_record(sh, &bf, rec_id(1), &dict).unwrap(), None);
        assert!(
            !dir.remove_record(sh, &bf, &mut alloc, rec_id(1), &dict)
                .unwrap()
        );
    }

    // A compression-off type's pages flow the same, just stored Raw with no
    // dictionary run ever allocated.
    #[test]
    fn compression_off_pages_flow_without_a_dictionary_run() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        let mut dict = DictState::new(false);

        dir.upsert_record(
            2,
            &bf,
            &mut alloc,
            rec_id(1),
            vec![0xCD; 100],
            &mut dict,
        )
        .unwrap();
        assert!(
            !dict.descriptor().is_allocated(),
            "compression off ⇒ no dictionary run"
        );
        assert_eq!(
            dir.get_record(2, &bf, rec_id(1), &dict).unwrap(),
            Some(vec![0xCD; 100])
        );
    }

    #[test]
    fn split_preserves_all_records() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        let mut dict = DictState::new(true);
        let sh = 1;

        for k in 0..200u64 {
            dir.upsert_record(
                sh,
                &bf,
                &mut alloc,
                rec_id(k),
                vec![k as u8; 4],
                &mut dict,
            )
            .unwrap();
        }
        assert_eq!(dir.len(), 1);

        dir.split_next(sh, &bf, &mut alloc, &dict).unwrap();
        assert_eq!(dir.len(), 2);

        // Every record still resolves after the repartition.
        for k in 0..200u64 {
            assert_eq!(
                dir.get_record(sh, &bf, rec_id(k), &dict).unwrap(),
                Some(vec![k as u8; 4]),
                "record {k} lost across split"
            );
        }
    }

    #[test]
    fn split_empty_bucket_is_noop_growth() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        let dict = DictState::new(true);
        dir.split_next(1, &bf, &mut alloc, &dict).unwrap();
        assert_eq!(dir.len(), 2);
    }
}
