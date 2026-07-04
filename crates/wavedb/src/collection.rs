//! The typed `NonUnique` client surface: `db.collection::<T>(pivot)` → a handle
//! whose `insert` / `get` / `save` / `remove` travel to the node as command
//! frames.
//!
//! Gated to a [`NonUniqueStruct`] whose generated handle is a [`PivotHandle`],
//! so a `Unique` type — which has no collection — can never open one.

use core::marker::PhantomData;

use wavedb_core::expose::Command;
use wavedb_core::wire::to_wire;
use wavedb_core::{Id, LocalId, NonUniqueStruct, PivotHandle};

use crate::db::Db;
use crate::error::Result;
use crate::reply;

impl Db {
    /// Open the collection of `T` referenced by `pivot` (a handle an owning
    /// record stored). Cheap — it captures the node handle and the root.
    pub fn collection<T>(&self, pivot: T::PivotId) -> ClientCollection<'_, T>
    where
        T: NonUniqueStruct,
        T::PivotId: PivotHandle,
    {
        ClientCollection {
            db: self,
            pivot: pivot.local_id(),
            _marker: PhantomData,
        }
    }
}

/// A typed handle to one tenant's collection of `T`, over a [`Db`].
///
/// Every mutation is one command to the node, which runs the authoritative
/// `Pivot`/`BpTree` engine; the client never walks the tree itself. Reads are
/// point (`get` by `Id`) for now — streaming walks (`all` / `by_<field>`) ship
/// with the transport's stream frames.
pub struct ClientCollection<'a, T> {
    db: &'a Db,
    pivot: LocalId,
    _marker: PhantomData<T>,
}

impl<T: NonUniqueStruct> ClientCollection<'_, T> {
    /// Insert `value`, minting and returning its stable identity `Id`.
    ///
    /// # Errors
    /// A failed call, or an unexpected reply.
    pub async fn insert(&self, value: T) -> Result<Id> {
        let payload = to_wire(&(self.pivot, value));
        let r = self
            .db
            .command(T::STRUCT_HASH, Command::Insert, payload)
            .await?;
        reply::inserted(&r)
    }

    /// Fetch the record at `id`. `None` = not in this collection.
    ///
    /// # Errors
    /// A failed call, or a decode fault.
    pub async fn get(&self, id: Id) -> Result<Option<T>> {
        let r = self
            .db
            .command(T::STRUCT_HASH, Command::Get, to_wire(&id))
            .await?;
        reply::value(r)
    }

    /// Update the record at `id` to `value` (its identity is unchanged; the
    /// previous version is chained node-side).
    ///
    /// # Errors
    /// A failed call, or an unexpected reply.
    pub async fn save(&self, id: Id, value: T) -> Result<()> {
        let payload = to_wire(&(id, value));
        let r = self
            .db
            .command(T::STRUCT_HASH, Command::Update, payload)
            .await?;
        reply::done(&r)
    }

    /// Remove the record at `id` (moved to the dead tree — bytes kept, history
    /// navigable). Returns whether it was in the living set.
    ///
    /// # Errors
    /// A failed call, or an unexpected reply.
    pub async fn remove(&self, id: Id) -> Result<bool> {
        let r = self
            .db
            .command(T::STRUCT_HASH, Command::Remove, to_wire(&id))
            .await?;
        reply::removed(&r)
    }
}
