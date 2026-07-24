//! `ServerDb` — the node-side execution context a `#[server]` function body
//! runs against.
//!
//! It implements [`DbHandle`] over the node's local [`Store`], so the same
//! generated spelling a client uses — `T::get(db)`, `T::collection(pivot)`,
//! `col.insert(db, v)` — resolves inside a server body without touching the
//! network. The `#[server]` macro retypes a body's `db: &Db` parameter to
//! `db: &ServerDb<S>`, so one body source drives both sides.
//!
//! Every op delegates to core's [`LocalHandle`], re-wrapping the error into
//! the client-facing [`Error`](crate::Error) so a body's `?` and the typed
//! helpers (`Error::not_found`, …) compose.

use futures::{Stream, TryStreamExt};
use wavedb_core::{
    Bound, DbHandle, Id, LocalHandle, LocalId, Metadata, NonUniqueStruct,
    Store, U48, UniqueStruct,
};

use crate::error::{Error, Result};

/// A node-side handle: a borrowed [`Store`] plus the bound identity. Cheap
/// to re-scope with [`as_tenant`](DbHandle::as_tenant).
pub struct ServerDb<'a, S> {
    local: LocalHandle<'a, S>,
    user: U48,
}

impl<'a, S: Store> ServerDb<'a, S> {
    /// Wrap a store + tenant as an execution context (`user = tenant` — the
    /// engine-local/B2C identity).
    pub const fn new(store: &'a S, tenant: U48) -> Self {
        Self {
            local: LocalHandle::new(store, tenant),
            user: tenant,
        }
    }

    /// The context a verified request executes as — what the generated
    /// `#[server]` dispatch builds from gate 1's [`Caller`].
    ///
    /// [`Caller`]: wavedb_core::Caller
    pub const fn for_caller(store: &'a S, caller: wavedb_core::Caller) -> Self {
        Self {
            local: LocalHandle::new(store, caller.tenant),
            user: caller.user,
        }
    }

    /// The verified user this context executes as (`U48::MAX` = the
    /// unauthenticated tier — only reachable inside `#[server(public)]`).
    #[must_use]
    pub const fn user(&self) -> U48 {
        self.user
    }
}

impl<S: Store> DbHandle for ServerDb<'_, S> {
    type Error = Error;

    fn tenant(&self) -> U48 {
        self.local.tenant()
    }

    fn as_tenant(&self, tenant: U48) -> Self {
        Self {
            user: self.user,
            local: self.local.as_tenant(tenant),
        }
    }

    async fn get_unique<T: UniqueStruct>(&self) -> Result<Option<T>> {
        Ok(self.local.get_unique().await?)
    }

    async fn save_unique<T: UniqueStruct>(&self, value: &T) -> Result<()> {
        Ok(self.local.save_unique(value).await?)
    }

    fn unique_history<T: UniqueStruct + 'static>(
        &self,
    ) -> impl Stream<Item = Result<(Metadata, T)>> {
        self.local.unique_history().map_err(Error::from)
    }

    async fn create_pivot<T: NonUniqueStruct>(&self) -> Result<LocalId> {
        Ok(self.local.create_pivot::<T>().await?)
    }

    async fn insert<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        value: &T,
    ) -> Result<Id> {
        Ok(self.local.insert(pivot, value).await?)
    }

    async fn get_record<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        id: Id,
    ) -> Result<Option<T>> {
        Ok(self.local.get_record(pivot, id).await?)
    }

    async fn update<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        id: Id,
        value: &T,
    ) -> Result<()> {
        Ok(self.local.update(pivot, id, value).await?)
    }

    async fn remove<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        id: Id,
    ) -> Result<bool> {
        Ok(self.local.remove::<T>(pivot, id).await?)
    }

    fn all<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
    ) -> impl Stream<Item = Result<T>> {
        self.local.all(pivot).map_err(Error::from)
    }

    fn search_by<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
        index: usize,
        bound: Bound,
    ) -> impl Stream<Item = Result<T>> {
        self.local
            .search_by(pivot, index, bound)
            .map_err(Error::from)
    }

    fn record_history<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
        id: Id,
    ) -> impl Stream<Item = Result<(Metadata, T)>> {
        self.local.record_history(pivot, id).map_err(Error::from)
    }
}
