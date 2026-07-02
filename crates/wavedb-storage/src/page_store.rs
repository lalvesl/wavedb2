//! [`PageStore`] — the node's authoritative [`Store`] backend.
//!
//! This is the disk-optimised key→value store the `Store`-generic
//! `Pivot`/`BpTree` layer in [`wavedb_core`] runs over. It ties together the
//! pieces built below it:
//!
//! - [`Journal`] — the write-ahead log; **durability is here** (`apply` fsyncs
//!   before it returns).
//! - an in-memory `BTreeMap<Id, bytes>` **cache** — reads serve from it directly,
//!   ordered by `Id`.
//! - one [`Directory`] per `STRUCT_HASH` over a [`BlockFile`] — the on-disk page
//!   layout (linear-hashed, homogeneous pages).
//!
//! ## Recovery model
//!
//! `data.bin` is a **deterministic projection of the journal**. On
//! [`open`](PageStore::open) the data file is truncated back to its superblock and
//! every committed batch is replayed through the same settle path that a live
//! `apply` uses — rebuilding the cache, the directories, and the allocator from
//! the log. So a crash loses nothing that was acked, and the page layout never has
//! to be checkpointed separately (that optimisation is deferred).
//!
//! The settle into pages is currently **inline** with `apply`; the design's
//! background settle/rebalance is a later optimisation, not a correctness need.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use parking_lot::Mutex;
use wavedb_core::{Id, Result as CoreResult, Store, Write};

use crate::block::BlockAllocator;
use crate::block_file::{BlockFile, RESERVED_BLOCKS};
use crate::directory::Directory;
use crate::error::{StorageError, StorageResult};

/// A bucket page spanning more blocks than this triggers one linear-hashing
/// `split_next` on its directory (round-robin), bounding page sizes. Tunable.
const DEFAULT_SPLIT_THRESHOLD_BLOCKS: u64 = 8; // 32 KiB

/// The native, page-backed [`Store`]. Cheap to clone behind an `Arc`; all mutable
/// state sits behind one mutex (the engine is single-writer per node).
#[derive(Debug)]
pub struct PageStore {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    file: BlockFile,
    journal: crate::journal::Journal,
    alloc: BlockAllocator,
    dirs: HashMap<u64, Directory>,
    cache: BTreeMap<u128, Vec<u8>>,
    seed: [u64; 4],
    split_threshold_blocks: u64,
}

impl PageStore {
    /// Open (or create) a node store rooted at directory `dir`, holding
    /// `data.bin` and `journal.log`. Replays the journal to rebuild state.
    ///
    /// # Errors
    /// [`StorageError`] on any filesystem / corruption fault during open or replay.
    pub fn open(dir: impl AsRef<Path>) -> StorageResult<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir)?;

        let file = BlockFile::open(dir.join("data.bin"))?;
        let seed = file.seed();
        // Rebuild pages from the journal: drop any prior page layout.
        file.truncate_to_blocks(RESERVED_BLOCKS)?;

        let mut alloc = BlockAllocator::new();
        alloc.alloc(RESERVED_BLOCKS); // reserve the superblock (block 0)

        let mut journal =
            crate::journal::Journal::open(dir.join("journal.log"))?;
        let batches = journal.replay()?;

        let mut inner = Inner {
            file,
            journal,
            alloc,
            dirs: HashMap::new(),
            cache: BTreeMap::new(),
            seed,
            split_threshold_blocks: DEFAULT_SPLIT_THRESHOLD_BLOCKS,
        };
        for batch in &batches {
            settle(&mut inner, batch)?; // rebuild cache + pages, do NOT re-journal
        }

        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    /// Number of live records currently cached.
    #[must_use]
    pub fn cache_len(&self) -> usize {
        self.inner.lock().cache.len()
    }

    /// Number of distinct `STRUCT_HASH` directories.
    #[must_use]
    pub fn directory_count(&self) -> usize {
        self.inner.lock().dirs.len()
    }

    /// Bucket count of one type's directory (`0` if the type is unseen).
    #[must_use]
    pub fn bucket_count(&self, struct_hash: u64) -> usize {
        self.inner
            .lock()
            .dirs
            .get(&struct_hash)
            .map_or(0, Directory::len)
    }

    /// Journal first (durable fsync), then settle into the cache and pages.
    fn apply_inner(&self, batch: &[Write]) -> StorageResult<()> {
        let mut inner = self.inner.lock();
        inner.journal.append(batch)?; // durability point
        let result = settle(&mut inner, batch);
        drop(inner); // release before returning — keep the critical section tight
        result
    }
}

impl Store for PageStore {
    async fn get(&self, id: Id) -> CoreResult<Option<Vec<u8>>> {
        Ok(self.inner.lock().cache.get(&id.raw()).cloned())
    }

    async fn apply(&self, batch: &[Write]) -> CoreResult<()> {
        self.apply_inner(batch)?; // StorageError → core::Error::Backend
        Ok(())
    }
}

/// Apply a batch to the cache and the on-disk pages (no journaling — the caller
/// journals first, and replay re-uses this path).
fn settle(inner: &mut Inner, batch: &[Write]) -> StorageResult<()> {
    let Inner {
        file,
        alloc,
        dirs,
        cache,
        seed,
        split_threshold_blocks,
        ..
    } = inner;

    for w in batch {
        match w {
            Write::Put(id, bytes) => {
                let sh = struct_hash_of(bytes)?;
                cache.insert(id.raw(), bytes.clone());
                let dir =
                    dirs.entry(sh).or_insert_with(|| Directory::new(*seed));
                dir.upsert_record(sh, file, alloc, *id, bytes.clone())?;
                maybe_split(
                    dir,
                    sh,
                    file,
                    alloc,
                    *split_threshold_blocks,
                    *id,
                )?;
            }
            Write::Remove(id) => {
                // Removal needs the record's STRUCT_HASH to find its directory;
                // recover it from the cached bytes before dropping them.
                if let Some(old) = cache.remove(&id.raw()) {
                    let sh = struct_hash_of(&old)?;
                    if let Some(dir) = dirs.get_mut(&sh) {
                        dir.remove_record(sh, file, alloc, *id)?;
                    }
                }
            }
        }
    }
    Ok(())
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
) -> StorageResult<()> {
    let touched = dir.bucket_of(id.raw());
    if dir.descriptor(touched).count() > threshold {
        dir.split_next(struct_hash, file, alloc)?;
    }
    Ok(())
}

/// The `STRUCT_HASH` at the head of a wire-encoded record (`[STRUCT_HASH][…]`).
fn struct_hash_of(bytes: &[u8]) -> StorageResult<u64> {
    if bytes.len() < 8 {
        return Err(StorageError::Corrupt("record shorter than STRUCT_HASH"));
    }
    Ok(u64::from_le_bytes(bytes[..8].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::PageStore;
    use futures::executor::block_on;
    use wavedb_core::{Id, Store, U48, Write};

    const SH: u64 = 0x1122_3344_5566_7788;

    /// A wire record: `[STRUCT_HASH (8 LE)][body]`.
    fn rec(struct_hash: u64, body: &[u8]) -> Vec<u8> {
        let mut v = struct_hash.to_le_bytes().to_vec();
        v.extend_from_slice(body);
        v
    }

    fn nonunique(key: u64) -> Id {
        Id::new(key, U48::from(1u32), false, (key & 0x7FFF) as u16)
    }

    #[test]
    fn put_get_remove() {
        let d = tempfile::tempdir().unwrap();
        let s = PageStore::open(d.path()).unwrap();
        block_on(async {
            assert_eq!(s.get(nonunique(1)).await.unwrap(), None);
            s.apply(&[Write::Put(nonunique(1), rec(SH, b"alpha"))])
                .await
                .unwrap();
            assert_eq!(
                s.get(nonunique(1)).await.unwrap(),
                Some(rec(SH, b"alpha"))
            );
            s.apply(&[Write::Remove(nonunique(1))]).await.unwrap();
            assert_eq!(s.get(nonunique(1)).await.unwrap(), None);
        });
    }

    #[test]
    fn batch_is_atomic_multi_record() {
        let d = tempfile::tempdir().unwrap();
        let s = PageStore::open(d.path()).unwrap();
        block_on(async {
            s.apply(&[
                Write::Put(nonunique(1), rec(SH, b"a")),
                Write::Put(nonunique(2), rec(SH, b"b")),
            ])
            .await
            .unwrap();
            assert_eq!(s.get(nonunique(1)).await.unwrap(), Some(rec(SH, b"a")));
            assert_eq!(s.get(nonunique(2)).await.unwrap(), Some(rec(SH, b"b")));
        });
    }

    #[test]
    fn survives_reopen() {
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            {
                let s = PageStore::open(d.path()).unwrap();
                s.apply(&[Write::Put(nonunique(7), rec(SH, b"durable"))])
                    .await
                    .unwrap();
            } // drop — no graceful flush beyond the journal fsync
            let s = PageStore::open(d.path()).unwrap();
            assert_eq!(
                s.get(nonunique(7)).await.unwrap(),
                Some(rec(SH, b"durable")),
                "journal must survive a reopen"
            );
        });
    }

    #[test]
    fn reopen_reflects_removals() {
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            {
                let s = PageStore::open(d.path()).unwrap();
                s.apply(&[Write::Put(nonunique(1), rec(SH, b"x"))])
                    .await
                    .unwrap();
                s.apply(&[Write::Remove(nonunique(1))]).await.unwrap();
            }
            let s = PageStore::open(d.path()).unwrap();
            assert_eq!(s.get(nonunique(1)).await.unwrap(), None);
            assert_eq!(s.cache_len(), 0);
        });
    }

    #[test]
    fn many_records_trigger_split_and_stay_readable() {
        let d = tempfile::tempdir().unwrap();
        let s = PageStore::open(d.path()).unwrap();
        block_on(async {
            // Each record ~1 KiB; enough of them overflow a bucket and split.
            for k in 0..400u64 {
                s.apply(&[Write::Put(
                    nonunique(k),
                    rec(SH, &vec![k as u8; 1024]),
                )])
                .await
                .unwrap();
            }
            assert!(
                s.bucket_count(SH) > 1,
                "expected at least one bucket split"
            );
            for k in 0..400u64 {
                assert_eq!(
                    s.get(nonunique(k)).await.unwrap(),
                    Some(rec(SH, &vec![k as u8; 1024])),
                    "record {k} lost"
                );
            }
        });
        // And it all survives a rebuild from the journal.
        let s = PageStore::open(d.path()).unwrap();
        block_on(async {
            assert_eq!(s.cache_len(), 400);
            assert_eq!(
                s.get(nonunique(399)).await.unwrap().unwrap().len(),
                8 + 1024
            );
        });
    }
}
