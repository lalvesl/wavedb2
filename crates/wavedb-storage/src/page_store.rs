//! [`PageStore`] — the node's authoritative [`Store`] backend.
//!
//! This is the disk-optimised key→value store the `Store`-generic
//! `Pivot`/`BpTree` layer in [`wavedb_core`] runs over. It ties together the
//! pieces built below it:
//!
//! - [`Journal`] — the write-ahead log; **durability is here** (`apply` fsyncs
//!   before it returns).
//! - one [`StructStorage`] **static per type** — that type's own in-memory
//!   cache (reads serve from it) and its own [`Directory`] over the shared
//!   [`BlockFile`]. The statics are `#[wavedb]`-generated and handed in as an
//!   explicit registry at [`open`](PageStore::open): compile-time state, no
//!   runtime `STRUCT_HASH → state` map.
//!
//! ## Locking
//!
//! There is no store-wide state lock. Each type's cache is its own `RwLock`
//! and its directory its own `Mutex`, so operations on different types run
//! concurrently and reads never wait on the settle path. The narrow globals:
//!
//! - `journal` (`Mutex`) — appends must be ordered; the cache commit happens
//!   under it so cache order always equals journal (= replay) order;
//! - `alloc` (`Mutex`) — block space is one shared resource.
//!
//! Lock order: `journal → cache(write)` on the commit path;
//! `dir → cache(read) → alloc` on the settle path. No path takes them in a
//! conflicting order, so the graph is acyclic.
//!
//! ## Recovery model
//!
//! `data.bin` is a **deterministic projection of the journal**. On
//! [`open`](PageStore::open) the data file is truncated back to its superblock
//! and every committed batch is replayed through the same commit + settle path
//! a live `apply` uses — rebuilding the caches, the directories, and the
//! allocator from the log. So a crash loses nothing that was acked. Settle
//! writes the **cache's current bytes** for a touched id (not the batch's),
//! which makes it order-independent: a late settle can only write newer state.
//!
//! The settle into pages is currently **inline** with `apply`; the design's
//! background settle/rebalance is a later optimisation, not a correctness need.

use std::path::Path;

use parking_lot::Mutex;
use wavedb_core::{Id, Result as CoreResult, Store, Write};

use crate::block::BlockAllocator;
use crate::block_file::{BlockFile, RESERVED_BLOCKS};
use crate::directory::Directory;
use crate::error::{StorageError, StorageResult};
use crate::struct_storage::{BPTREE_NODE_STORAGE, EngineClaim, StructStorage};

/// A bucket page spanning more blocks than this triggers one linear-hashing
/// `split_next` on its directory (round-robin), bounding page sizes. Tunable.
const DEFAULT_SPLIT_THRESHOLD_BLOCKS: u64 = 8; // 32 KiB

/// Ids one batch touched, grouped by registry slot — what the settle consumes.
type Touched = Vec<(usize, Vec<Id>)>;

/// The native, page-backed [`Store`]. One instance per process (the node
/// model: one process, one `data.bin`); state lives in the per-type
/// [`StructStorage`] statics registered at [`open`](Self::open).
#[derive(Debug)]
pub struct PageStore {
    file: BlockFile,
    journal: Mutex<crate::journal::Journal>,
    alloc: Mutex<BlockAllocator>,
    /// The registered slots, sorted by `STRUCT_HASH` (deduped) — a lock-free,
    /// read-only route table after open.
    types: Vec<&'static StructStorage>,
    seed: [u64; 4],
    split_threshold_blocks: u64,
    _claim: EngineClaim,
}

impl PageStore {
    /// Open (or create) a node store rooted at directory `dir`, holding
    /// `data.bin` and `journal.log`, serving exactly the listed `types` (each
    /// a `#[wavedb]`-generated static — `T::storage_entries()`). The reserved
    /// `BpTree`-node slot is always included. Replays the journal to rebuild
    /// every slot's cache and pages.
    ///
    /// The registry is an allowlist: a write whose `STRUCT_HASH` has no listed
    /// slot fails with [`StorageError::UnregisteredStructHash`].
    ///
    /// # Errors
    /// [`StorageError::EngineBusy`] if this process already has an open store;
    /// otherwise any filesystem / corruption fault during open or replay.
    pub fn open(
        dir: impl AsRef<Path>,
        types: &[&'static StructStorage],
    ) -> StorageResult<Self> {
        let claim = EngineClaim::acquire()?;
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

        // Generated Pivot types of identical shape may share a STRUCT_HASH;
        // they also share pages today, so one slot serves the hash — dedup.
        let mut types: Vec<&'static StructStorage> = types.to_vec();
        types.push(&BPTREE_NODE_STORAGE);
        types.sort_by_key(|s| s.struct_hash());
        types.dedup_by_key(|s| s.struct_hash());
        for slot in &types {
            slot.reset(); // a prior run's state (same process) must not leak in
        }

        let store = Self {
            file,
            journal: Mutex::new(journal),
            alloc: Mutex::new(alloc),
            types,
            seed,
            split_threshold_blocks: DEFAULT_SPLIT_THRESHOLD_BLOCKS,
            _claim: claim,
        };
        for batch in &batches {
            store.route_batch(batch)?; // rebuild caches + pages, no re-journal
            let touched = store.commit_to_caches(batch);
            store.settle(&touched)?;
        }
        Ok(store)
    }

    /// Number of live records currently cached across every registered type.
    #[must_use]
    pub fn cache_len(&self) -> usize {
        self.types.iter().map(|s| s.cached_len()).sum()
    }

    /// Bucket count of one type's directory (`0` while nothing settled).
    #[must_use]
    pub fn bucket_count(&self, struct_hash: u64) -> usize {
        self.slot_of(struct_hash).map_or(0, |slot| {
            slot.directory().lock().as_ref().map_or(0, Directory::len)
        })
    }

    /// The registered slot for `struct_hash`, if listed at open.
    fn slot_of(&self, struct_hash: u64) -> Option<&'static StructStorage> {
        self.types
            .binary_search_by_key(&struct_hash, |s| s.struct_hash())
            .ok()
            .map(|i| self.types[i])
    }

    /// Journal first (durable fsync), then commit to the per-type caches and
    /// settle into their pages.
    // The journal guard deliberately spans the cache commit — releasing it
    // earlier (the lint's suggestion) would let two applies commit their
    // caches in the opposite order to their journal frames, so replay could
    // disagree with what live readers saw.
    #[allow(clippy::significant_drop_tightening)]
    fn apply_inner(&self, batch: &[Write]) -> StorageResult<()> {
        // Refuse unroutable writes *before* the journal sees the batch, so the
        // log never holds a batch replay would choke on.
        self.route_batch(batch)?;
        let touched = {
            let mut journal = self.journal.lock();
            journal.append(batch)?; // durability point
            // Commit under the journal lock: cache order == journal order.
            self.commit_to_caches(batch)
        };
        self.settle(&touched)
    }

    /// Verify every `Put` in `batch` routes to a registered slot.
    fn route_batch(&self, batch: &[Write]) -> StorageResult<()> {
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
    fn commit_to_caches(&self, batch: &[Write]) -> Touched {
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
                    note_touched(&mut touched, idx, *id);
                }
                Write::Remove(id) => {
                    // The id alone names no type: the owning slot is whichever
                    // cache holds it (writers are serialised by the journal
                    // lock, so the check-then-remove pair can't race).
                    let owner = self.types.iter().position(|s| {
                        s.mem_cache().read().contains_key(&id.raw())
                    });
                    if let Some(idx) = owner {
                        self.types[idx].mem_cache().write().remove(&id.raw());
                        note_touched(&mut touched, idx, *id);
                    }
                }
            }
        }
        touched
    }

    /// Converge the touched ids' pages to the caches' current state: present
    /// in cache ⇒ upsert those bytes, absent ⇒ remove. Writing cache state
    /// (not batch state) makes settling idempotent and order-independent.
    // The directory + dictionary guards must span one slot's whole settle
    // loop — both are read and rewritten across every id; the lint's "merge
    // into single usage" would drop them mid-mutation.
    #[allow(clippy::significant_drop_tightening)]
    fn settle(&self, touched: &Touched) -> StorageResult<()> {
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

impl Store for PageStore {
    async fn get(&self, id: Id) -> CoreResult<Option<Vec<u8>>> {
        // Untyped fallback: the id alone names no type, so probe every slot.
        // Typed callers use `get_of` and skip this scan entirely.
        Ok(self.types.iter().find_map(|s| s.get(id)))
    }

    async fn get_of(
        &self,
        struct_hash: u64,
        id: Id,
    ) -> CoreResult<Option<Vec<u8>>> {
        // One binary search on the route table, then only this type's own
        // cache read lock — reads of different types never contend.
        Ok(self.slot_of(struct_hash).and_then(|s| s.get(id)))
    }

    async fn apply(&self, batch: &[Write]) -> CoreResult<()> {
        self.apply_inner(batch)?; // StorageError → core::Error::Backend
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
    dict: &crate::dictionary::DictState,
) -> StorageResult<()> {
    let touched = dir.bucket_of(id.raw());
    if dir.descriptor(touched).count() > threshold {
        dir.split_next(struct_hash, file, alloc, dict)?;
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
    use crate::error::StorageError;
    use crate::struct_storage::StructStorage;
    use futures::executor::block_on;
    use parking_lot::{Mutex, MutexGuard};
    use wavedb_core::{Id, Store, U48, Write};

    const SH: u64 = 0x1122_3344_5566_7788;

    /// The test slot every raw record routes to (`SH`-headed).
    static TEST_SLOT: StructStorage = StructStorage::new(SH);

    /// The per-struct slots are process-global statics, so only one store may
    /// live at a time — serialise the tests that open one.
    fn engine_gate() -> MutexGuard<'static, ()> {
        static GATE: Mutex<()> = Mutex::new(());
        GATE.lock()
    }

    fn open(path: &std::path::Path) -> PageStore {
        PageStore::open(path, &[&TEST_SLOT]).unwrap()
    }

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
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path());
        block_on(async {
            assert_eq!(s.get(nonunique(1)).await.unwrap(), None);
            s.apply(&[Write::Put(nonunique(1), rec(SH, b"alpha"))])
                .await
                .unwrap();
            assert_eq!(
                s.get(nonunique(1)).await.unwrap(),
                Some(rec(SH, b"alpha"))
            );
            // The typed path reaches the same bytes through the slot directly.
            assert_eq!(
                s.get_of(SH, nonunique(1)).await.unwrap(),
                Some(rec(SH, b"alpha"))
            );
            assert_eq!(s.get_of(SH ^ 1, nonunique(1)).await.unwrap(), None);
            s.apply(&[Write::Remove(nonunique(1))]).await.unwrap();
            assert_eq!(s.get(nonunique(1)).await.unwrap(), None);
        });
    }

    #[test]
    fn batch_is_atomic_multi_record() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path());
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
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            {
                let s = open(d.path());
                s.apply(&[Write::Put(nonunique(7), rec(SH, b"durable"))])
                    .await
                    .unwrap();
            } // drop — no graceful flush beyond the journal fsync
            let s = open(d.path());
            assert_eq!(
                s.get(nonunique(7)).await.unwrap(),
                Some(rec(SH, b"durable")),
                "journal must survive a reopen"
            );
        });
    }

    #[test]
    fn reopen_reflects_removals() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            {
                let s = open(d.path());
                s.apply(&[Write::Put(nonunique(1), rec(SH, b"x"))])
                    .await
                    .unwrap();
                s.apply(&[Write::Remove(nonunique(1))]).await.unwrap();
            }
            let s = open(d.path());
            assert_eq!(s.get(nonunique(1)).await.unwrap(), None);
            assert_eq!(s.cache_len(), 0);
        });
    }

    #[test]
    fn unregistered_struct_hash_is_refused_before_journaling() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path());
        block_on(async {
            let err = s
                .apply(&[Write::Put(nonunique(1), rec(SH ^ 0xFF, b"stray"))])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("no StructStorage registered"));
            // Nothing journaled: a reopen replays cleanly and stays empty.
        });
        drop(s);
        let s = open(d.path());
        assert_eq!(s.cache_len(), 0, "refused write must not reach the log");
    }

    #[test]
    fn second_open_in_process_is_engine_busy() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        let first = open(d.path());
        let d2 = tempfile::tempdir().unwrap();
        assert!(matches!(
            PageStore::open(d2.path(), &[&TEST_SLOT]).unwrap_err(),
            StorageError::EngineBusy
        ));
        drop(first);
        // Slot released on drop — opening again succeeds.
        let _second = open(d.path());
    }

    /// ~1 KiB of pseudo-random (zstd-incompressible) bytes — pages must grow
    /// past the split threshold by their **stored** (compressed) size, so
    /// compressible filler would never trigger a split.
    fn noise(k: u64) -> Vec<u8> {
        let mut state = k.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..1024)
            .map(|_| {
                // xorshift64
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    #[test]
    fn many_records_trigger_split_and_stay_readable() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path());
        block_on(async {
            // Each record ~1 KiB; enough of them overflow a bucket and split.
            for k in 0..200u64 {
                s.apply(&[Write::Put(nonunique(k), rec(SH, &noise(k)))])
                    .await
                    .unwrap();
            }
            assert!(
                s.bucket_count(SH) > 1,
                "expected at least one bucket split"
            );
            for k in 0..200u64 {
                assert_eq!(
                    s.get(nonunique(k)).await.unwrap(),
                    Some(rec(SH, &noise(k))),
                    "record {k} lost"
                );
            }
        });
        drop(s);
        // And it all survives a rebuild from the journal.
        let s = open(d.path());
        block_on(async {
            assert_eq!(s.cache_len(), 200);
            assert_eq!(
                s.get(nonunique(199)).await.unwrap().unwrap().len(),
                8 + 1024
            );
        });
    }
}
