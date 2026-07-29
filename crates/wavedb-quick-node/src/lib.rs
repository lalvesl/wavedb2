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
mod poll;
mod serve;
mod serve_ws;
mod subscribe;

use std::cell::RefCell;
use std::future::{Future, pending};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;

use tokio::net::TcpListener;
use wavedb_core::expose::Exposure;
use wavedb_storage::{PageStore, StorageRegistry};

pub use error::{Result, ServerError};

use subscribe::{NotifyStore, SubTable};

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
    secret: Option<[u8; 32]>,
}

/// The background maintenance policy: how the node settles, checkpoints,
/// and bounds its caches while serving.
#[derive(Debug, Clone, Copy)]
struct Maintenance {
    /// Journal bytes that trigger a checkpoint (journal truncates to zero).
    checkpoint_after_bytes: u64,
    /// Cache bytes the settle task evicts down to (settled entries only).
    cache_budget_bytes: usize,
    /// How long a poll-watch session survives without polling before its
    /// event buffer is dropped.
    poll_session_ttl: std::time::Duration,
}

impl Default for Maintenance {
    fn default() -> Self {
        Self {
            checkpoint_after_bytes: 64 * 1024 * 1024, // 64 MiB of journal
            cache_budget_bytes: 1024 * 1024 * 1024,   // 1 GiB — generous
            poll_session_ttl: std::time::Duration::from_mins(1),
        }
    }
}

/// A node that has opened its engine and bound its socket — ready to
/// [`run`](Bound::run). Splitting bind from run lets a caller read the
/// [`local_addr`](Bound::local_addr) first (tests bind port 0).
pub struct Bound<E> {
    registry: E,
    listener: TcpListener,
    /// The raw engine — maintenance, node-side seeding, and the final commit
    /// drive it directly (the notify wrapper is built per-serve around a
    /// clone of this `Rc`).
    store: Rc<PageStore>,
    /// Shared with the serve-time [`NotifyStore`]; the WS session loops
    /// register their subscriptions here.
    subs: Rc<RefCell<SubTable>>,
    /// The HTTP poll-watch buffers — fed by the same [`NotifyStore`],
    /// drained by the sync route, pruned by maintenance.
    polls: subscribe::Polls,
    maintenance: Maintenance,
    secret: [u8; 32],
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
            secret: None,
        }
    }

    /// The token-signing secret (HMAC key). Without one, a fresh random
    /// secret is drawn at [`bind`](Self::bind) — fine for a single node
    /// (tokens simply die on restart); set it explicitly to survive
    /// restarts or share across processes.
    #[must_use]
    pub const fn secret(mut self, secret: [u8; 32]) -> Self {
        self.secret = Some(secret);
        self
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
        // Publish for the token-minting helpers (`wavedb::auth`) — one node
        // per process, like the engine's storage slots. The secret is
        // whatever the *first* open installed: an in-process reopen keeps
        // verifying the tokens it already issued.
        wavedb_net::auth::set_node_secret(
            self.secret.unwrap_or_else(random_secret),
        );
        let secret = *wavedb_net::auth::node_secret()
            .unwrap_or_else(|| unreachable!("just installed"));
        let subs = Rc::new(RefCell::new(SubTable::default()));
        let polls = Rc::new(RefCell::new(poll::PollTable::default()));
        Ok(Bound {
            registry: self.registry,
            listener,
            store: Rc::new(store),
            subs,
            polls,
            maintenance: self.maintenance,
            secret,
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
    /// Seeding writes bypass the notify wrapper, so they raise no live
    /// events — subscriptions predate serving anyway.
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
        // The serving store publishes mutations; maintenance + the final
        // commit drive the raw engine underneath it (same `Rc`).
        let serving = Rc::new(NotifyStore::new(
            Rc::clone(&self.store),
            Rc::clone(&self.subs),
            Rc::clone(&self.polls),
        ));
        serve::run(
            self.listener,
            self.registry,
            serving,
            self.secret,
            Rc::clone(&self.subs),
            Rc::clone(&self.polls),
            maintain(
                Rc::clone(&self.store),
                Rc::clone(&self.polls),
                self.maintenance,
            ),
            shutdown,
        )
        .await?;
        // Clean shutdown: everything settled + committed — a restart
        // replays nothing.
        self.store.commit_journal()?;
        Ok(())
    }
}

/// A fresh random 32-byte secret from platform entropy
/// (`wavedb_platform::rand`, infallible natively — and a node is always
/// native). Default for a node given no explicit secret.
fn random_secret() -> [u8; 32] {
    wavedb_platform::rand::secret32()
        .unwrap_or_else(|_| unreachable!("native entropy is infallible"))
}

/// The background maintenance loop: periodically settle the pending queue,
/// checkpoint once the journal crosses the threshold, evict settled cache
/// entries down to budget, and age out poll-watch sessions that stopped
/// polling. An engine fault stops maintenance (acked writes stay safe in
/// the journal); serving continues.
async fn maintain(
    store: Rc<PageStore>,
    polls: subscribe::Polls,
    policy: Maintenance,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if store.drain().is_err() {
            return;
        }
        if store.journal_len() > policy.checkpoint_after_bytes
            && store.commit_journal().is_err()
        {
            return;
        }
        store.evict_settled(policy.cache_budget_bytes);
        polls.borrow_mut().prune(policy.poll_session_ttl);
    }
}
