//! Pivot-owned shards ([RFC 0064]) — the concurrency model, first slice.
//!
//! ```text
//!   route by pivot ──> shard i   (owns its cache; runs Collection/BpTree)
//!                        │
//!                        │  DiskRequest  (the only Send surface)
//!                        ▼
//!                   disk actor   (sole owner of the PageStore)
//! ```
//!
//! **What is built here:** the ownership boundary. One disk actor owns the
//! engine, shards reach it only by message, each shard keeps its own cache,
//! and `shard_of(pivot)` decides which shard an operation belongs to. Nothing
//! above `Store` changes.
//!
//! **What is not, and why.** RFC 0064's target splits the disk owner into a
//! journal actor and a page actor, and moves `StructStorage`'s per-type cache
//! into the shards. Both wait on the same open question — the settle path
//! straddles the boundary, reading shard-side caches to write page-side
//! dictionaries — so this slice draws the boundary where it can be drawn
//! correctly and leaves that seam alone.
//!
//! The shard count `N` is [`wavedb_platform::cpu::available`] by default: the
//! parallelism this process may actually use, which is the affinity mask and
//! the container quota rather than the machine's core count.
//!
//! [RFC 0064]: ../../../../rfcs/0064-pivot-owned-concurrency-PLANNED.md

mod disk;
mod lock;
mod msg;
pub mod priority;
mod route;
mod store;

pub use disk::{
    DiskHandle, MAINTENANCE_DEPTH, QUEUE_DEPTH, start as start_disk,
};
pub use lock::{OwnerKey, OwnerLocks};
pub use msg::{DiskRequest, EngineStats, Maintenance};
pub use priority::Priority;
pub use route::{Owner, shard_of};
pub use store::{CACHE_BYTES, ShardStore};

use std::rc::Rc;

use wavedb_storage::PageStore;

/// A running set of shards over one engine.
///
/// Holds the disk handle and the shard count; a shard's own `ShardStore` is
/// built **on that shard's thread** by [`Shards::store`], because it is
/// non-`Send` and must not be constructed anywhere it could be moved from.
pub struct Shards {
    disk: DiskHandle,
    count: usize,
}

impl Shards {
    /// Take ownership of `store` into a disk actor and prepare `count` shards.
    ///
    /// `count` is clamped to at least 1: zero shards is not a degraded mode,
    /// it is a process that serves nothing.
    ///
    /// # Errors
    /// The disk actor's thread failing to spawn.
    pub fn start(
        store: PageStore,
        count: usize,
    ) -> wavedb_platform::Result<Self> {
        Ok(Self {
            disk: disk::start(store)?,
            count: count.max(1),
        })
    }

    /// Shards sized to the parallelism this process actually has.
    ///
    /// # Errors
    /// The disk actor's thread failing to spawn.
    pub fn start_sized(store: PageStore) -> wavedb_platform::Result<Self> {
        Self::start(store, wavedb_platform::cpu::available())
    }

    /// How many shards this process runs.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The shard owning `pivot`.
    #[must_use]
    pub fn owner(&self, pivot: wavedb_core::LocalId) -> usize {
        shard_of(pivot, self.count)
    }

    /// Build a shard's store. Call this **on the shard's own thread** — the
    /// returned value is non-`Send` by construction and holds that thread's
    /// cache.
    #[must_use]
    pub fn store(&self) -> Rc<ShardStore> {
        Rc::new(ShardStore::new(self.disk.clone()))
    }

    /// A handle for a thread that will build its own store later.
    #[must_use]
    pub fn handle(&self) -> DiskHandle {
        self.disk.clone()
    }

    /// Ask for low-priority maintenance. Returns immediately — the caller is
    /// by definition not waiting, which is what makes it low priority.
    pub fn hint(&self, work: Maintenance) {
        self.disk.hint(work);
    }
}
