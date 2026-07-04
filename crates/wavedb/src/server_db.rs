//! `ServerDb` — the node-side execution context a `#[server]` function body
//! runs against.
//!
//! It mirrors the client [`Db`](crate::Db) typed surface (`get` / `save` /
//! `history` / `collection` / `create_pivot` / `as_tenant`) but resolves every
//! call **locally**, against the node's `Store`, instead of over the network.
//! The `#[server]` macro retypes a body's `db: &Db` parameter to
//! `db: &ServerDb<S>`, so the same body source drives both sides.

use wavedb_core::{
    Collection, Id, NonUniqueStruct, PivotHandle, Store, U48, UniqueStruct,
};

use crate::error::Result;

/// A node-side handle: a borrowed [`Store`] plus the bound tenant. Cheap to
/// re-scope with [`as_tenant`](Self::as_tenant).
pub struct ServerDb<'a, S> {
    store: &'a S,
    tenant: U48,
}

impl<'a, S: Store> ServerDb<'a, S> {
    /// Wrap a store + tenant as an execution context.
    pub const fn new(store: &'a S, tenant: U48) -> Self {
        Self { store, tenant }
    }

    /// The tenant this context is bound to.
    pub const fn tenant(&self) -> U48 {
        self.tenant
    }

    /// The same store, scoped to a different tenant — the cross-tenant seam a
    /// `register`-style function uses to bootstrap a new tenant's records.
    #[must_use]
    pub const fn as_tenant(&self, tenant: U48) -> Self {
        Self {
            store: self.store,
            tenant,
        }
    }

    /// Fetch this tenant's `Unique` record of type `T`.
    ///
    /// # Errors
    /// A store failure or a decode fault.
    pub async fn get<T: UniqueStruct>(&self) -> Result<Option<T>> {
        Ok(
            wavedb_core::collection::get_unique(self.store, self.tenant)
                .await?,
        )
    }

    /// Save (upsert) this tenant's `Unique` record.
    ///
    /// # Errors
    /// A store failure.
    pub async fn save<T: UniqueStruct>(&self, value: &T) -> Result<()> {
        wavedb_core::collection::save_unique(self.store, self.tenant, value)
            .await?;
        Ok(())
    }

    /// This tenant's `Unique` record versions, newest-first.
    ///
    /// # Errors
    /// A store failure or a decode fault.
    pub async fn history<T: UniqueStruct>(&self) -> Result<Vec<T>> {
        use futures::TryStreamExt;
        let versions: Vec<(wavedb_core::Metadata, T)> =
            wavedb_core::collection::unique_history(self.store, self.tenant)
                .try_collect()
                .await?;
        Ok(versions.into_iter().map(|(_, v)| v).collect())
    }

    /// Create a new, empty collection of `T` under this tenant, returning its
    /// handle (store it in an owning record).
    ///
    /// # Errors
    /// A store failure.
    pub async fn create_pivot<T>(&self) -> Result<T::PivotId>
    where
        T: NonUniqueStruct,
        T::PivotId: PivotHandle,
    {
        let root = Collection::<T>::create(self.store, self.tenant).await?;
        Ok(T::PivotId::from_local_id(root))
    }

    /// Open the collection of `T` referenced by `pivot`.
    pub fn collection<T>(&self, pivot: T::PivotId) -> ServerCollection<'a, S, T>
    where
        T: NonUniqueStruct,
        T::PivotId: PivotHandle,
    {
        ServerCollection {
            col: Collection::at(pivot.local_id(), self.tenant),
            store: self.store,
        }
    }
}

/// A node-side collection handle — the local counterpart of the client's
/// `ClientCollection`, driving the core [`Collection`] against the store.
pub struct ServerCollection<'a, S, T: NonUniqueStruct> {
    col: Collection<T>,
    store: &'a S,
}

impl<S: Store, T: NonUniqueStruct> ServerCollection<'_, S, T> {
    /// Insert `value`, returning its stable identity `Id`.
    ///
    /// # Errors
    /// A store failure.
    pub async fn insert(&self, value: T) -> Result<Id> {
        Ok(self.col.insert(self.store, &value).await?)
    }

    /// Fetch the record at `id`.
    ///
    /// # Errors
    /// A store failure or a decode fault.
    pub async fn get(&self, id: Id) -> Result<Option<T>> {
        Ok(self.col.get(self.store, id).await?)
    }

    /// Update the record at `id` to `value`.
    ///
    /// # Errors
    /// A store failure.
    pub async fn save(&self, id: Id, value: T) -> Result<()> {
        self.col.save(self.store, id, &value).await?;
        Ok(())
    }

    /// Remove the record at `id`; returns whether it was in the living set.
    ///
    /// # Errors
    /// A store failure.
    pub async fn remove(&self, id: Id) -> Result<bool> {
        Ok(self.col.remove(self.store, id).await?)
    }

    /// Every living record, in `CREATED_AT` order.
    ///
    /// # Errors
    /// A store failure or a decode fault.
    pub async fn all(&self) -> Result<Vec<T>> {
        use futures::TryStreamExt;
        let items: Vec<(Id, T)> =
            self.col.all(self.store).try_collect().await?;
        Ok(items.into_iter().map(|(_, v)| v).collect())
    }
}
