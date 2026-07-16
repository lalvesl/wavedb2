//! `wavedb-quick-node` — the serving + storage node.
//!
//! **Server and database are the same binary.** A node links a schema
//! crate's `expose_server!` output (`REGISTRY`) — which carries both halves
//! it needs: the dispatch surface ([`Exposure`]) and the storage surface
//! ([`StorageRegistry`], the [`StructStorage`] slots to open the engine with)
//! — binds an HTTP POST socket, and serves records straight out of
//! [`PageStore`].
//!
//! ```no_run
//! # async fn run<E>(registry: E) -> wavedb_quick_node::Result<()>
//! # where E: wavedb_core::expose::Exposure
//! #     + wavedb_storage::StorageRegistry + Copy + 'static {
//! wavedb_quick_node::Server::new(registry)
//!     .data_dir("./data")
//!     .serve("0.0.0.0:7700")
//!     .await
//! # }
//! ```
//!
//! **Single node for now.** Durability is the journal (a write is durable
//! once journaled). The ring / gossip / replication / failover machinery the
//! README describes is the target design, deferred.
//!
//! [`Exposure`]: wavedb_core::expose::Exposure
//! [`StorageRegistry`]: wavedb_storage::StorageRegistry
//! [`StructStorage`]: wavedb_storage::StructStorage

// The node serves connections on a single-thread `LocalSet` (see `serve`), so
// the `Store`-generic engine futures are deliberately non-`Send` — an
// internal node seam, not a public `Send`-bounded API. Same stance
// `wavedb-core` and `wavedb-storage` take with their engine seams.
#![allow(clippy::future_not_send)]

pub mod dispatch;
pub mod error;
mod serve;

use std::future::{Future, pending};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;

use tokio::net::TcpListener;
use wavedb_core::expose::Exposure;
use wavedb_storage::{PageStore, StorageRegistry};

pub use error::{Result, ServerError};

/// A node, configured but not yet bound.
///
/// `E` is a schema crate's `expose_server!` registry: it is both the
/// [`Exposure`](wavedb_core::expose::Exposure) dispatch surface and the
/// [`StorageRegistry`] that names the engine's per-type slots.
#[derive(Debug, Clone)]
pub struct Server<E> {
    registry: E,
    data_dir: PathBuf,
    maintenance: Maintenance,
}

/// The background maintenance policy: how the node settles, checkpoints,
/// and bounds its caches while serving.
#[derive(Debug, Clone, Copy)]
struct Maintenance {
    /// Journal bytes that trigger a checkpoint (journal truncates to zero).
    checkpoint_after_bytes: u64,
    /// Cache bytes the settle task evicts down to (settled entries only).
    cache_budget_bytes: usize,
}

impl Default for Maintenance {
    fn default() -> Self {
        Self {
            checkpoint_after_bytes: 64 * 1024 * 1024, // 64 MiB of journal
            cache_budget_bytes: 1024 * 1024 * 1024,   // 1 GiB — generous
        }
    }
}

/// A node that has opened its engine and bound its socket — ready to
/// [`run`](Bound::run). Splitting bind from run lets a caller read the
/// [`local_addr`](Bound::local_addr) first (tests bind port 0).
pub struct Bound<E> {
    registry: E,
    listener: TcpListener,
    store: Rc<PageStore>,
    maintenance: Maintenance,
}

impl<E> Server<E>
where
    E: Exposure + StorageRegistry + Copy + 'static,
{
    /// Configure a node around a schema registry. Data goes to `./data`
    /// until [`data_dir`](Self::data_dir) says otherwise.
    #[must_use]
    pub fn new(registry: E) -> Self {
        Self {
            registry,
            data_dir: PathBuf::from("data"),
            maintenance: Maintenance::default(),
        }
    }

    /// Set the directory holding `data.bin` + `journal.log`.
    #[must_use]
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.data_dir = dir.into();
        self
    }

    /// Checkpoint (persist the pages' metadata and truncate the journal)
    /// once the journal exceeds `bytes`. Default 64 MiB.
    #[must_use]
    pub const fn checkpoint_after_bytes(mut self, bytes: u64) -> Self {
        self.maintenance.checkpoint_after_bytes = bytes;
        self
    }

    /// Evict settled cache entries down to `bytes` (reads then serve from
    /// the pages). Default 1 GiB.
    #[must_use]
    pub const fn cache_budget_bytes(mut self, bytes: usize) -> Self {
        self.maintenance.cache_budget_bytes = bytes;
        self
    }

    /// Open the engine and bind the listener, without yet accepting.
    ///
    /// # Errors
    /// [`ServerError::Storage`] if the engine can't open (busy, corruption),
    /// [`ServerError::Io`] if the socket can't bind.
    pub async fn bind(self, addr: &str) -> Result<Bound<E>> {
        let store =
            PageStore::open(&self.data_dir, &self.registry.storage_entries())?;
        let listener = TcpListener::bind(addr).await?;
        Ok(Bound {
            registry: self.registry,
            listener,
            store: Rc::new(store),
            maintenance: self.maintenance,
        })
    }

    /// Open, bind, and serve until the listener faults — the one-call path.
    ///
    /// # Errors
    /// As [`bind`](Self::bind), plus a fatal accept fault while serving.
    pub async fn serve(self, addr: &str) -> Result<()> {
        self.bind(addr).await?.run().await
    }
}

impl<E> Bound<E>
where
    E: Exposure + Copy + 'static,
{
    /// The address the listener actually bound (resolves an `:0` request).
    ///
    /// # Errors
    /// [`ServerError::Io`] if the socket address can't be read.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.listener.local_addr()?)
    }

    /// The opened engine — direct access for node-side seeding (e.g. creating
    /// a collection `Pivot` before serving, or admin tooling). Ordinary
    /// requests never touch this; they route through [`run`](Self::run).
    #[must_use]
    pub fn store(&self) -> &PageStore {
        &self.store
    }

    /// Accept and serve connections until the listener faults (runs forever
    /// under normal operation). A background maintenance task settles
    /// queued writes into pages, checkpoints past the journal threshold,
    /// and holds the caches to budget.
    ///
    /// # Errors
    /// [`ServerError::Net`] on a fatal accept fault.
    pub async fn run(self) -> Result<()> {
        self.run_with_shutdown(pending()).await
    }

    /// Accept and serve connections until `shutdown` resolves, then settle
    /// and checkpoint (a clean restart replays nothing) and return —
    /// dropping the engine, which releases the process-wide store claim.
    ///
    /// # Errors
    /// [`ServerError::Net`] on a fatal accept fault; [`ServerError::Storage`]
    /// if the final checkpoint fails.
    pub async fn run_with_shutdown(
        self,
        shutdown: impl Future<Output = ()>,
    ) -> Result<()> {
        let store = Rc::clone(&self.store);
        serve::run(
            self.listener,
            self.registry,
            Rc::clone(&store),
            maintain(store, self.maintenance),
            shutdown,
        )
        .await?;
        // Clean shutdown: everything settled + checkpointed, journal empty.
        self.store.checkpoint()?;
        Ok(())
    }
}

/// The background maintenance loop: periodically settle the pending queue,
/// checkpoint once the journal crosses the threshold, and evict settled
/// cache entries down to budget. An engine fault stops maintenance (acked
/// writes stay safe in the journal); serving continues.
async fn maintain(store: Rc<PageStore>, policy: Maintenance) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if store.drain().is_err() {
            return;
        }
        if store.journal_len() > policy.checkpoint_after_bytes
            && store.checkpoint().is_err()
        {
            return;
        }
        store.evict_settled(policy.cache_budget_bytes);
    }
}
