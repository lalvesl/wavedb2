//! [`Directory`] page **reads** — resolving a bucket to its [`SlotPage`] over
//! a [`BlockFile`].
//!
//! The addressing math and the container live in [`crate::directory`]. The
//! write half is not here: pages are rewritten by the checkpoint
//! ([`crate::plan`] builds the images, [`crate::checkpoint`] places them all
//! in one window), so no single-record page rewrite path exists.

use wavedb_core::Id;

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
}

#[cfg(test)]
mod tests {
    use crate::block::{BlockDescriptor, Run};
    use crate::block_file::{BlockFile, RESERVED_BLOCKS};
    use crate::dictionary::DictState;
    use crate::directory::Directory;
    use crate::page::SlotPage;
    use wavedb_core::{Id, U48};

    const SH: u64 = 0xABCD;

    fn rec_id(key: u64) -> Id {
        Id::new(key, U48::from(1u32), false, (key & 0x7FFF) as u16)
    }

    /// A page written by hand (what the checkpoint's window does in bulk)
    /// resolves back through the directory's read path.
    #[test]
    fn a_placed_page_resolves_through_the_directory() {
        let d = tempfile::tempdir().unwrap();
        let file = BlockFile::open(d.path().join("data.bin")).unwrap();
        let dict = DictState::new(true);
        let mut dir = Directory::new(file.seed());

        let mut page = SlotPage::new(SH);
        page.upsert(rec_id(1), vec![1, 2, 3]);
        page.upsert(rec_id(2), vec![4, 5]);
        let bytes = page.to_bytes(dict.dictionary(), dict.enabled()).unwrap();

        let run = Run::new(RESERVED_BLOCKS, 1);
        file.write_run(run, &bytes).unwrap();
        dir.set_descriptor(
            0,
            BlockDescriptor::from_run_used(run, bytes.len() as u64),
        );

        assert_eq!(
            dir.get_record(SH, &file, rec_id(1), &dict).unwrap(),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            dir.get_record(SH, &file, rec_id(2), &dict).unwrap(),
            Some(vec![4, 5])
        );
        assert_eq!(dir.get_record(SH, &file, rec_id(9), &dict).unwrap(), None);
    }

    /// An unallocated bucket reads as an empty page, never as a fault.
    #[test]
    fn vacant_bucket_reads_empty() {
        let d = tempfile::tempdir().unwrap();
        let file = BlockFile::open(d.path().join("data.bin")).unwrap();
        let dict = DictState::new(true);
        let dir = Directory::new(file.seed());
        assert!(dir.read_page(SH, &file, 0, &dict).unwrap().is_empty());
        assert_eq!(dir.get_record(SH, &file, rec_id(1), &dict).unwrap(), None);
    }
}
