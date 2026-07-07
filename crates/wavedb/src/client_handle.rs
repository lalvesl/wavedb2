//! [`DbHandle`] for the client [`Db`]: every typed op becomes a command
//! frame to the node.
//!
//! This is what makes the generated `T::get(&db)` / `col.insert(&db, v)`
//! spelling work against the network — the same call sites that resolve
//! against a `LocalHandle` or a `ServerDb`.
//!
//! Walk-shaped ops are **streamed**: the node writes one frame per record
//! and the client decodes each as it arrives — a caller can stop early
//! without the node's whole answer in memory client-side. Three ops have
//! **no wire command yet** and refuse with the node's uniform answer,
//! [`UnknownStructHash`](wavedb_core::Error::UnknownStructHash):
//! `create_pivot` (collections bootstrap inside `#[server]` bodies),
//! `search_by`, and `record_history`.

use futures::{Stream, StreamExt, TryStreamExt};
use wavedb_core::expose::Command;
use wavedb_core::wire::{WaveWire, from_wire, to_wire, to_wire_pair};
use wavedb_core::{
    Bound, DbHandle, Id, LocalId, Metadata, NonUniqueStruct, U48, UniqueStruct,
};

use crate::db::Db;
use crate::error::{Error, Result};
use crate::reply;

/// Run a walk-shaped command and decode each streamed item frame as a `V`.
// `Db` is re-exported; `pub(crate)` keeps this seam crate-internal.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn streamed<'a, V: WaveWire + 'a>(
    db: &'a Db,
    struct_hash: u64,
    command: Command,
    payload: Vec<u8>,
) -> impl Stream<Item = Result<V>> + 'a {
    futures::stream::once(db.command_stream(struct_hash, command, payload))
        .map(|res| match res {
            Ok(items) => items.left_stream(),
            Err(e) => {
                futures::stream::once(std::future::ready(Err(e))).right_stream()
            }
        })
        .flatten()
        .and_then(|bytes| async move {
            from_wire::<V>(&bytes)
                .map_err(|e| Error::Core(wavedb_core::Error::from(e)))
        })
}

/// The uniform refusal for an op the wire cannot carry yet — the same answer
/// the node gives an unlisted hash.
fn refuse<T>(struct_hash: u64) -> impl Stream<Item = Result<T>> {
    futures::stream::once(std::future::ready(Err(Error::Core(
        wavedb_core::Error::UnknownStructHash(struct_hash),
    ))))
}

impl DbHandle for Db {
    type Error = Error;

    fn tenant(&self) -> U48 {
        // The inherent accessor (method resolution prefers it at call sites;
        // the trait impl delegates so both agree).
        Self::tenant(self)
    }

    fn as_tenant(&self, tenant: U48) -> Self {
        Self::as_tenant(self, tenant)
    }

    async fn get_unique<T: UniqueStruct>(&self) -> Result<Option<T>> {
        let r = self
            .command(T::STRUCT_HASH, Command::Get, Vec::new())
            .await?;
        reply::value(r)
    }

    async fn save_unique<T: UniqueStruct>(&self, value: &T) -> Result<()> {
        let r = self
            .command(T::STRUCT_HASH, Command::Save, to_wire(value))
            .await?;
        reply::done(&r)
    }

    fn unique_history<T: UniqueStruct + 'static>(
        &self,
    ) -> impl Stream<Item = Result<(Metadata, T)>> {
        // Each item frame carries one `(Metadata, T)` version pair.
        streamed(self, T::STRUCT_HASH, Command::History, Vec::new())
    }

    async fn create_pivot<T: NonUniqueStruct>(&self) -> Result<LocalId> {
        // Not wire-reachable: a collection is bootstrapped node-side (inside
        // a `#[server]` body via `ServerDb`), never by a raw client command.
        Err(Error::Core(wavedb_core::Error::UnknownStructHash(
            <T::Pivot as wavedb_core::Pivot>::STRUCT_HASH,
        )))
    }

    async fn insert<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        value: &T,
    ) -> Result<Id> {
        // `(pivot, value)` as the wire tuple the node's insert step decodes.
        let payload = to_wire_pair(&pivot, value);
        let r = self
            .command(T::STRUCT_HASH, Command::Insert, payload)
            .await?;
        reply::inserted(&r)
    }

    async fn get_record<T: NonUniqueStruct>(
        &self,
        _pivot: LocalId,
        id: Id,
    ) -> Result<Option<T>> {
        let r = self
            .command(T::STRUCT_HASH, Command::Get, to_wire(&id))
            .await?;
        reply::value(r)
    }

    async fn update<T: NonUniqueStruct>(
        &self,
        _pivot: LocalId,
        id: Id,
        value: &T,
    ) -> Result<()> {
        // The node reaches the collection through the record's
        // `Metadata.pivot_id` back-link — the handle's pivot never travels.
        let payload = to_wire_pair(&id, value);
        let r = self
            .command(T::STRUCT_HASH, Command::Update, payload)
            .await?;
        reply::done(&r)
    }

    async fn remove<T: NonUniqueStruct>(
        &self,
        _pivot: LocalId,
        id: Id,
    ) -> Result<bool> {
        let r = self
            .command(T::STRUCT_HASH, Command::Remove, to_wire(&id))
            .await?;
        reply::removed(&r)
    }

    fn all<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
    ) -> impl Stream<Item = Result<T>> {
        streamed(self, T::STRUCT_HASH, Command::All, to_wire(&pivot))
    }

    fn search_by<T: NonUniqueStruct + 'static>(
        &self,
        _pivot: LocalId,
        _index: usize,
        _bound: Bound,
    ) -> impl Stream<Item = Result<T>> {
        // No wire command yet — secondary lookups over the transport land
        // with the streaming frames.
        refuse(T::STRUCT_HASH)
    }

    fn record_history<T: NonUniqueStruct + 'static>(
        &self,
        _pivot: LocalId,
        _id: Id,
    ) -> impl Stream<Item = Result<(Metadata, T)>> {
        // No wire command yet — the NonUnique timeline walk is node-side
        // only until a `History`-by-id frame exists.
        refuse(T::STRUCT_HASH)
    }
}
