//! [`PageStore`] — the node's authoritative [`Store`] backend.
//!
//! This is the disk-optimised key→value store the `Store`-generic
//! `Pivot`/`BpTree` layer in [`wavedb_core`] runs over. It ties together the
//! pieces built below it:
//!
//! - [`Journal`](crate::journal::Journal) — the write-ahead log;
//!   **durability is here** (`apply` fsyncs before it returns).
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
//! concurrently and a cached read never waits on the settle path. The narrow
//! globals:
//!
//! - `journal` (`Mutex`) — appends must be ordered; the cache commit happens
//!   under it so cache order always equals journal (= replay) order;
//! - `alloc` (`Mutex`) — block space is one shared resource.
//!
//! Lock order: `journal → pending → dir → cache` on the commit path (the
//! directory is only taken to route a `Remove` whose owner is no longer
//! cached; the pending push stays under the journal so eviction's
//! "queue empty ⇒ everything settled" check can't race a commit);
//! `pending → dir → cache(read) → alloc` on the settle drain; `dir → dict`
//! on the read-through path. No path takes them in a conflicting order, so
//! the graph is acyclic.
//!
//! ## Reads
//!
//! The cache is a **cache**, not the dataset: a read serves from the type's
//! cache when it holds the id and falls back to the settled page
//! ([`Directory::get_record`]) otherwise. Settle writes the cache's current
//! bytes before anything is evicted, so the fallback can only ever see the
//! newest settled state — never a version the cache has superseded. An
//! absent id costs one page probe; that's fine until a keyed filter earns
//! its place.
//!
//! ## Recovery model
//!
//! `data.bin` is a **deterministic projection of the journal**; recovery
//! roots in the newest valid `Commit` frame (journal-rooted — see
//! [`PageStore::commit_journal`] and the `commit` module docs). On
//! [`open`](PageStore::open): with a durable `Commit`, the
//! directories/dictionaries/allocator load from its chains, the caches start
//! **empty** (reads fall through to the pages), and only the uncovered
//! `Batch` frames replay; without one (first generation), the data file is
//! truncated back to its superblock and every committed batch replays
//! through the same commit + settle path a live `apply` uses. Either way a
//! crash loses nothing that was acked. Settle writes the **cache's current
//! bytes** for a touched id (not the batch's), which makes it
//! order-independent and replay-idempotent: a late or repeated settle can
//! only write newer state.
//!
//! The settle into pages is **deferred**: `apply` commits to the journal +
//! caches and queues the touched ids; [`drain`](PageStore::drain) (driven by
//! the node's maintenance task) writes the pages later. An unsettled remove
//! is tombstoned so the read fallback never resurrects the page's stale
//! bytes. [`PageStore::commit_journal`] drains first, so the frame it
//! appends always covers everything committed.

use std::path::Path;

use parking_lot::Mutex;
use wavedb_core::{Id, Result as CoreResult, Store, Write};

use crate::block::BlockAllocator;
use crate::block_file::BlockFile;
use crate::directory::Directory;
use crate::error::{StorageError, StorageResult};
use crate::struct_storage::{BPTREE_NODE_STORAGE, EngineClaim, StructStorage};

/// A bucket page spanning more blocks than this triggers one linear-hashing
/// `split_next` on its directory (round-robin), bounding page sizes. Tunable.
const DEFAULT_SPLIT_THRESHOLD_BLOCKS: u64 = 8; // 32 KiB

/// Ids one batch touched, grouped by registry slot — what the settle consumes.
pub(crate) type Touched = Vec<(usize, Vec<Id>)>;

/// The native, page-backed [`Store`]. One instance per process (the node
/// model: one process, one `data.bin`); state lives in the per-type
/// [`StructStorage`] statics registered at [`open`](Self::open).
#[derive(Debug)]
pub struct PageStore {
    pub(crate) file: BlockFile,
    /// The directory holding `data.bin` + the journal chain (rotation
    /// creates new journal files here).
    pub(crate) data_dir: std::path::PathBuf,
    pub(crate) journal: Mutex<crate::journal::Journal>,
    pub(crate) alloc: Mutex<BlockAllocator>,
    /// The registered slots, sorted by `STRUCT_HASH` (deduped) — a lock-free,
    /// read-only route table after open.
    pub(crate) types: Vec<&'static StructStorage>,
    pub(crate) seed: [u64; 4],
    pub(crate) split_threshold_blocks: u64,
    /// Touched ids committed to the caches but not yet settled into pages —
    /// what [`drain`](Self::drain) consumes.
    pub(crate) pending: Mutex<Touched>,
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

        let data_bin_existed = dir.join("data.bin").exists();
        let file = BlockFile::open(dir.join("data.bin"))?;
        let seed = file.seed();

        // Generated Pivot types of identical shape may share a STRUCT_HASH;
        // they also share pages today, so one slot serves the hash — dedup.
        let mut types: Vec<&'static StructStorage> = types.to_vec();
        types.push(&BPTREE_NODE_STORAGE);
        types.sort_by_key(|s| s.struct_hash());
        types.dedup_by_key(|s| s.struct_hash());
        for slot in &types {
            slot.reset(); // a prior run's state (same process) must not leak in
        }

        // Recover from the journal chain: the newest `Commit` frame roots
        // the directories/dictionaries/allocator (caches stay empty — reads
        // fall through to the pages); every uncovered `Batch` replays below.
        let recovered =
            crate::commit::restore(dir, data_bin_existed, &file, &types)?;

        let store = Self {
            file,
            data_dir: dir.to_path_buf(),
            journal: Mutex::new(recovered.journal),
            alloc: Mutex::new(recovered.alloc),
            types,
            seed,
            split_threshold_blocks: DEFAULT_SPLIT_THRESHOLD_BLOCKS,
            pending: Mutex::new(Vec::new()),
            _claim: claim,
        };
        for batch in &recovered.replay {
            store.route_batch(batch)?; // rebuild caches + pages, no re-journal
            let touched = store.commit_to_caches(batch)?;
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
        journal.append(&crate::journal::JournalFrame::Batch(batch.to_vec()))?; // durability point
        // Commit under the journal lock: cache order == journal order.
        let touched = self.commit_to_caches(batch)?;
        merge_touched(&mut self.pending.lock(), touched);
        Ok(())
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
    ///
    /// A page probe (routing a `Remove` whose owner is not cached) can fail
    /// on a disk fault — *after* the durability point. The live state then
    /// under-applies the batch, but the journal holds it whole: a reopen
    /// replays it correctly, which is the strongest promise a broken disk
    /// leaves available.
    fn commit_to_caches(&self, batch: &[Write]) -> StorageResult<Touched> {
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
    fn evicted_records_read_through_from_pages() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path());
        block_on(async {
            s.apply(&[
                Write::Put(nonunique(1), rec(SH, b"settled")),
                Write::Put(nonunique(2), rec(SH, b"also")),
            ])
            .await
            .unwrap();
            s.drain().unwrap(); // settle into pages
            // Simulate eviction: drop the cache; the settled pages must serve.
            TEST_SLOT.mem_cache().write().clear();
            assert_eq!(
                s.get_of(SH, nonunique(1)).await.unwrap(),
                Some(rec(SH, b"settled")),
                "typed read must fall through to the page"
            );
            assert_eq!(
                s.get(nonunique(2)).await.unwrap(),
                Some(rec(SH, b"also")),
                "untyped read must fall through to the page"
            );
            // Absent ids stay absent through the fallback.
            assert_eq!(s.get(nonunique(99)).await.unwrap(), None);
            assert_eq!(s.get_of(SH, nonunique(99)).await.unwrap(), None);
        });
    }

    #[test]
    fn remove_of_evicted_record_reaches_its_page() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            {
                let s = open(d.path());
                s.apply(&[Write::Put(nonunique(3), rec(SH, b"gone soon"))])
                    .await
                    .unwrap();
                s.drain().unwrap();
                // Evict, then remove: the owner must be found on the page.
                TEST_SLOT.mem_cache().write().clear();
                s.apply(&[Write::Remove(nonunique(3))]).await.unwrap();
                // Before the settle lands, the tombstone must hide the
                // (still-present) page bytes…
                assert_eq!(s.get(nonunique(3)).await.unwrap(), None);
                assert_eq!(s.get_of(SH, nonunique(3)).await.unwrap(), None);
                // …and after it, the page itself agrees.
                s.drain().unwrap();
                assert_eq!(s.get(nonunique(3)).await.unwrap(), None);
            }
            // And the journal agrees on replay.
            let s = open(d.path());
            assert_eq!(s.get(nonunique(3)).await.unwrap(), None);
            assert_eq!(s.cache_len(), 0);
        });
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

    #[test]
    fn unsettled_writes_read_correctly_and_survive_reopen() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            {
                let s = open(d.path());
                s.apply(&[Write::Put(nonunique(1), rec(SH, b"queued"))])
                    .await
                    .unwrap();
                assert!(s.has_pending(), "settle must be deferred");
                assert_eq!(
                    s.get(nonunique(1)).await.unwrap(),
                    Some(rec(SH, b"queued")),
                    "the cache serves an unsettled write"
                );
                assert_eq!(s.bucket_count(SH), 0, "no page written yet");
            } // dropped with the queue never drained
            let s = open(d.path());
            assert_eq!(
                s.get(nonunique(1)).await.unwrap(),
                Some(rec(SH, b"queued")),
                "the journal covers unsettled writes"
            );
        });
    }

    #[test]
    fn tombstone_hides_stale_page_until_settle() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path());
        block_on(async {
            s.apply(&[Write::Put(nonunique(9), rec(SH, b"v1"))])
                .await
                .unwrap();
            s.drain().unwrap(); // page holds v1
            s.apply(&[Write::Remove(nonunique(9))]).await.unwrap();
            // Unsettled remove: the page still holds v1; reads must not
            // resurrect it.
            assert_eq!(s.get(nonunique(9)).await.unwrap(), None);
            // A re-put supersedes the tombstone immediately.
            s.apply(&[Write::Put(nonunique(9), rec(SH, b"v2"))])
                .await
                .unwrap();
            assert_eq!(
                s.get(nonunique(9)).await.unwrap(),
                Some(rec(SH, b"v2"))
            );
            s.drain().unwrap();
            assert_eq!(
                s.get(nonunique(9)).await.unwrap(),
                Some(rec(SH, b"v2"))
            );
        });
    }

    #[test]
    fn evict_settled_respects_budget_and_pending() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        let s = open(d.path());
        block_on(async {
            s.apply(&[Write::Put(nonunique(1), rec(SH, b"resident"))])
                .await
                .unwrap();
            // Pending queue non-empty ⇒ eviction must refuse.
            s.evict_settled(0);
            assert!(s.cache_len() > 0, "unsettled entries must not evict");
            s.drain().unwrap();
            s.evict_settled(0);
            assert_eq!(s.cache_len(), 0, "settled entries evict to budget");
            assert_eq!(
                s.get(nonunique(1)).await.unwrap(),
                Some(rec(SH, b"resident")),
                "evicted reads fall through to the page"
            );
        });
    }

    /// The journal files under a store dir, sorted by ts.
    fn journals(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        crate::journal::scan(dir)
            .unwrap()
            .into_iter()
            .map(|(_, p)| p)
            .collect()
    }

    #[test]
    fn commit_retires_the_old_journal_and_reopen_is_cold() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            {
                let s = open(d.path());
                s.apply(&[Write::Put(nonunique(1), rec(SH, b"kept"))])
                    .await
                    .unwrap();
                s.apply(&[
                    Write::Put(nonunique(2), rec(SH, b"doomed")),
                    Write::Put(nonunique(3), rec(SH, b"kept too")),
                ])
                .await
                .unwrap();
                s.apply(&[Write::Remove(nonunique(2))]).await.unwrap();
                s.commit_journal().unwrap();
                assert_eq!(
                    journals(d.path()).len(),
                    1,
                    "the retired journal must be deleted"
                );
            }
            let s = open(d.path());
            assert_eq!(s.cache_len(), 0, "a committed open starts cold");
            assert_eq!(
                s.get(nonunique(1)).await.unwrap(),
                Some(rec(SH, b"kept"))
            );
            assert_eq!(
                s.get_of(SH, nonunique(3)).await.unwrap(),
                Some(rec(SH, b"kept too"))
            );
            assert_eq!(
                s.get(nonunique(2)).await.unwrap(),
                None,
                "a pre-commit remove must hold after restore"
            );
        });
    }

    #[test]
    fn writes_after_commit_replay_over_it() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            {
                let s = open(d.path());
                s.apply(&[Write::Put(nonunique(1), rec(SH, b"old"))])
                    .await
                    .unwrap();
                s.commit_journal().unwrap();
                // Post-commit traffic lands in the fresh journal.
                s.apply(&[Write::Put(nonunique(1), rec(SH, b"new"))])
                    .await
                    .unwrap();
                s.apply(&[Write::Put(nonunique(4), rec(SH, b"tail"))])
                    .await
                    .unwrap();
            }
            let s = open(d.path());
            assert_eq!(
                s.get(nonunique(1)).await.unwrap(),
                Some(rec(SH, b"new")),
                "the journal tail must win over committed state"
            );
            assert_eq!(
                s.get(nonunique(4)).await.unwrap(),
                Some(rec(SH, b"tail"))
            );
        });
    }

    #[test]
    fn covered_journal_left_behind_is_skipped_and_cleaned() {
        // Simulates a crash between the Commit frame and the old journal's
        // delete: the covered journal is still on disk; recovery must skip
        // it (its frames are already in data.bin) and clean it up.
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            let (old_path, old_bytes) = {
                let s = open(d.path());
                s.apply(&[Write::Put(nonunique(1), rec(SH, b"v1"))])
                    .await
                    .unwrap();
                s.apply(&[Write::Put(nonunique(1), rec(SH, b"v2"))])
                    .await
                    .unwrap();
                s.apply(&[Write::Put(nonunique(5), rec(SH, b"other"))])
                    .await
                    .unwrap();
                s.apply(&[Write::Remove(nonunique(5))]).await.unwrap();
                let old = journals(d.path()).pop().unwrap();
                let bytes = std::fs::read(&old).unwrap();
                s.commit_journal().unwrap();
                (old, bytes)
            };
            // "Un-delete" the covered journal.
            std::fs::write(&old_path, &old_bytes).unwrap();
            assert_eq!(journals(d.path()).len(), 2);
            let s = open(d.path());
            assert_eq!(
                s.get(nonunique(1)).await.unwrap(),
                Some(rec(SH, b"v2"))
            );
            assert_eq!(s.get(nonunique(5)).await.unwrap(), None);
            assert_eq!(
                journals(d.path()).len(),
                1,
                "recovery must clean the covered leftover"
            );
        });
    }

    #[test]
    fn torn_commit_frame_falls_back_to_the_old_journal() {
        // Simulates a crash mid-Commit-append: the frame is torn (crc
        // fails) and the retired journal was never deleted — recovery must
        // ignore the torn commit and rebuild from the old journal's frames.
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            let (old_path, old_bytes) = {
                let s = open(d.path());
                s.apply(&[Write::Put(nonunique(7), rec(SH, b"survives"))])
                    .await
                    .unwrap();
                let old = journals(d.path()).pop().unwrap();
                let bytes = std::fs::read(&old).unwrap();
                s.commit_journal().unwrap();
                (old, bytes)
            };
            // Tear the Commit frame (drop its last byte) and un-delete the
            // old journal — the exact crash-mid-append disk state.
            let new_path = journals(d.path()).pop().unwrap();
            let len = std::fs::metadata(&new_path).unwrap().len();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&new_path)
                .unwrap()
                .set_len(len - 1)
                .unwrap();
            std::fs::write(&old_path, &old_bytes).unwrap();
            let s = open(d.path());
            assert_eq!(
                s.get(nonunique(7)).await.unwrap(),
                Some(rec(SH, b"survives")),
                "the old journal must rule when the commit is torn"
            );
        });
    }

    #[test]
    fn missing_journal_with_data_refuses_to_open() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        block_on(async {
            let s = open(d.path());
            s.apply(&[Write::Put(nonunique(1), rec(SH, b"x"))])
                .await
                .unwrap();
        });
        for j in journals(d.path()) {
            std::fs::remove_file(j).unwrap();
        }
        assert!(matches!(
            PageStore::open(d.path(), &[&TEST_SLOT]).unwrap_err(),
            StorageError::Corrupt(_)
        ));
    }

    #[test]
    fn commit_restores_dictionary_compressed_pages() {
        let _g = engine_gate();
        let d = tempfile::tempdir().unwrap();
        // Repetitive bodies → the dictionary warms and pages store Zstd, so
        // reading them back after restore needs the persisted dictionary.
        let body = |k: u64| {
            let mut v = vec![b'A'; 1024];
            v.extend_from_slice(&k.to_le_bytes());
            rec(SH, &v)
        };
        block_on(async {
            {
                let s = open(d.path());
                for k in 0..50u64 {
                    s.apply(&[Write::Put(nonunique(k), body(k))])
                        .await
                        .unwrap();
                }
                s.commit_journal().unwrap();
            }
            let s = open(d.path());
            assert_eq!(s.cache_len(), 0);
            for k in 0..50u64 {
                assert_eq!(
                    s.get_of(SH, nonunique(k)).await.unwrap(),
                    Some(body(k)),
                    "record {k} must decompress against the restored dict"
                );
            }
            // The store keeps working: settle + a second commit over the
            // restored state (untouched roots carry forward).
            s.apply(&[Write::Put(nonunique(60), body(60))])
                .await
                .unwrap();
            s.commit_journal().unwrap();
        });
        // Third generation open still serves everything.
        block_on(async {
            let s = open(d.path());
            assert!(s.get_of(SH, nonunique(60)).await.unwrap().is_some());
            assert_eq!(
                s.get_of(SH, nonunique(0)).await.unwrap(),
                Some(body(0))
            );
        });
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
            s.drain().unwrap(); // splits happen on the settle path
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
