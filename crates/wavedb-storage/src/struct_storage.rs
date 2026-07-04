//! [`StructStorage`] — one struct's own storage slot: its in-memory cache and
//! its page [`Directory`], each behind its **own** lock.
//!
//! This is the compile-time replacement for a runtime `STRUCT_HASH → state`
//! map: `#[wavedb]` emits one `static StructStorage` per declared type (native
//! targets only), so the state a type's operations touch is resolved by the
//! compiler, and two types never contend on a shared lock. The [`PageStore`]
//! receives the statics as an explicit registry at open — declared, not
//! discovered, exactly like the exposure lists.
//!
//! Because the slots are process-global statics, **one process drives one open
//! [`PageStore`] at a time** (the node model: one process, one `data.bin`).
//! [`PageStore::open`] enforces it.
//!
//! [`PageStore`]: crate::page_store::PageStore
//! [`PageStore::open`]: crate::page_store::PageStore::open

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};
use wavedb_core::Id;
use wavedb_core::index::BPTREE_NODE_STRUCT_HASH;

use crate::dictionary::DictState;
use crate::directory::Directory;
use crate::error::{StorageError, StorageResult};

/// One type's in-memory record cache: `Id.raw → wire bytes`, ordered by `Id`.
pub type StructMemCache = RwLock<BTreeMap<u128, Vec<u8>>>;

/// One type's page-directory slot. `None` until the open [`PageStore`] binds it
/// (the [`Directory`] needs the per-database seed, which only exists at open).
///
/// [`PageStore`]: crate::page_store::PageStore
pub type StructDirectory = Mutex<Option<Directory>>;

/// One type's compression slot: its zstd policy + dictionary + persisted run.
/// Per-type by nature — a page holds one type, so it compresses against that
/// type's own dictionary and nothing else.
pub type StructDictionary = Mutex<DictState>;

/// One struct's storage state — its own cache, directory, and compression
/// dictionary, with a lock per part. `const`-constructible so the `#[wavedb]`
/// macro can emit it as a `static` on the declared type.
#[derive(Debug)]
pub struct StructStorage {
    struct_hash: u64,
    cache: StructMemCache,
    dir: StructDirectory,
    dict: StructDictionary,
}

impl StructStorage {
    /// A slot for `struct_hash`, pages compressed (the record/Pivot default).
    #[must_use]
    pub const fn new(struct_hash: u64) -> Self {
        Self {
            struct_hash,
            cache: RwLock::new(BTreeMap::new()),
            dir: Mutex::new(None),
            dict: Mutex::new(DictState::new(true)),
        }
    }

    /// A slot whose pages never run through zstd — for hot, constantly
    /// rewritten page kinds where the CPU spend doesn't pay (`BpTree` nodes),
    /// or a `#[wavedb(compress = false)]` type.
    #[must_use]
    pub const fn without_compression(struct_hash: u64) -> Self {
        Self {
            struct_hash,
            cache: RwLock::new(BTreeMap::new()),
            dir: Mutex::new(None),
            dict: Mutex::new(DictState::new(false)),
        }
    }

    /// The `STRUCT_HASH` this slot stores.
    #[must_use]
    pub const fn struct_hash(&self) -> u64 {
        self.struct_hash
    }

    /// Whether this type's pages compress (the policy is fixed at the slot's
    /// `const` construction; only the dictionary contents change at runtime).
    #[must_use]
    pub fn compress(&self) -> bool {
        self.dict.lock().enabled()
    }

    /// This type's in-memory cache — its own `RwLock`, shared with no other
    /// type, so reads of different types never contend.
    #[must_use]
    pub const fn mem_cache(&self) -> &StructMemCache {
        &self.cache
    }

    /// This type's page-directory slot — its own `Mutex`, shared with no other
    /// type. `None` until an open [`PageStore`] settles the first value.
    ///
    /// [`PageStore`]: crate::page_store::PageStore
    #[must_use]
    pub const fn directory(&self) -> &StructDirectory {
        &self.dir
    }

    /// This type's compression slot — its own `Mutex`, shared with no other
    /// type: the zstd policy, the raw-content dictionary, and the block run
    /// the dictionary is persisted in.
    #[must_use]
    pub const fn dictionary(&self) -> &StructDictionary {
        &self.dict
    }

    /// The cached bytes at `id`, if present (a read lock on this type only).
    #[must_use]
    pub fn get(&self, id: Id) -> Option<Vec<u8>> {
        self.cache.read().get(&id.raw()).cloned()
    }

    /// Number of records currently cached for this type.
    #[must_use]
    pub fn cached_len(&self) -> usize {
        self.cache.read().len()
    }

    /// Drop all state (policy kept) — the open path calls this before a
    /// journal replay so a prior run's cache/directory/dictionary (same
    /// process, e.g. tests) can't leak in.
    pub(crate) fn reset(&self) {
        self.cache.write().clear();
        *self.dir.lock() = None;
        self.dict.lock().reset();
    }
}

/// The storage half of a node registry — the [`StructStorage`] slots a node
/// must register at [`PageStore::open`].
///
/// `expose_server!` emits this impl for its zero-sized `ServerRegistry`
/// (native targets only), flattening every listed type's
/// `storage_entries()`. So a node's `.registry(REGISTRY)` alone carries both
/// halves: the dispatch surface ([`Exposure`]) *and* the storage surface —
/// declared once, not discovered.
///
/// The reserved [`BPTREE_NODE_STORAGE`] slot is **not** returned here;
/// [`PageStore::open`] adds it automatically.
///
/// [`Exposure`]: wavedb_core::expose::Exposure
/// [`PageStore::open`]: crate::page_store::PageStore::open
pub trait StorageRegistry {
    /// The slots for every declared type (record + any generated Pivot),
    /// flattened into one registry list for [`PageStore::open`].
    ///
    /// [`PageStore::open`]: crate::page_store::PageStore::open
    fn storage_entries(&self) -> Vec<&'static StructStorage>;
}

/// The reserved slot every `BpTree` node value settles into.
///
/// Compression off: node pages are rewritten on every index mutation, so zstd
/// there is CPU for nothing. [`PageStore::open`] registers it automatically —
/// callers list only their own types.
///
/// [`PageStore::open`]: crate::page_store::PageStore::open
pub static BPTREE_NODE_STORAGE: StructStorage =
    StructStorage::without_compression(BPTREE_NODE_STRUCT_HASH);

/// `true` while a `PageStore` is open in this process. The per-struct slots
/// above are process-global statics, so two live stores would corrupt each
/// other — the second open fails with [`StorageError::EngineBusy`].
static ENGINE_OPEN: AtomicBool = AtomicBool::new(false);

/// Releases the process-wide engine slot when the store drops (also on an
/// `open` that fails after claiming).
#[derive(Debug)]
pub(crate) struct EngineClaim;

impl EngineClaim {
    pub(crate) fn acquire() -> StorageResult<Self> {
        ENGINE_OPEN
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .map_err(|_| StorageError::EngineBusy)?;
        Ok(Self)
    }
}

impl Drop for EngineClaim {
    fn drop(&mut self) {
        ENGINE_OPEN.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::{BPTREE_NODE_STORAGE, StructStorage};
    use wavedb_core::index::BPTREE_NODE_STRUCT_HASH;
    use wavedb_core::{Id, U48};

    static SLOT: StructStorage = StructStorage::new(0xABCD);

    fn id(key: u64) -> Id {
        Id::new(key, U48::from(1u32), false, 0)
    }

    #[test]
    fn static_slot_caches_and_resets() {
        SLOT.reset(); // statics are process-global; start clean
        assert_eq!(SLOT.struct_hash(), 0xABCD);
        assert!(SLOT.compress());
        assert_eq!(SLOT.get(id(1)), None);

        SLOT.mem_cache().write().insert(id(1).raw(), vec![7, 8]);
        assert_eq!(SLOT.get(id(1)), Some(vec![7, 8]));
        assert_eq!(SLOT.cached_len(), 1);

        SLOT.reset();
        assert_eq!(SLOT.get(id(1)), None);
        assert!(SLOT.directory().lock().is_none());
    }

    #[test]
    fn node_slot_is_reserved_and_uncompressed() {
        assert_eq!(BPTREE_NODE_STORAGE.struct_hash(), BPTREE_NODE_STRUCT_HASH);
        assert!(!BPTREE_NODE_STORAGE.compress());
    }
}
