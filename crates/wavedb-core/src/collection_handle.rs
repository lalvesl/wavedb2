//! [`CollectionHandle`] — the typed collection surface application code
//! drives, generic over its execution context.
//!
//! `#[wavedb(NonUnique)]` emits `T::collection(pivot_id)` returning one of
//! these; every method re-takes the [`DbHandle`] so the same handle value
//! works against a `LocalHandle`, the client `Db`, or a `ServerDb`:
//!
//! ```text
//! let todos = Todo::create_pivot(&db).await?;      // once, explicit
//! let col   = Todo::collection(todos);             // cheap, Copy
//! let id    = col.insert(&db, &Todo { .. }).await?;
//! col.all(&db)                                     // Stream<Result<Todo>>
//! ```
//!
//! The engine-facing [`Collection`](crate::Collection) (which takes a
//! [`Store`](crate::Store) directly) stays the internal layer this surface
//! resolves to on local contexts.

use std::marker::PhantomData;

use futures::Stream;

use crate::handle::DbHandle;
use crate::id::Id;
use crate::index::Bound;
use crate::local_id::LocalId;
use crate::metadata::Metadata;
use crate::traits::NonUniqueStruct;

/// The typed handle into one NonUnique collection, addressed by its `Pivot`
/// record's [`LocalId`]. Holds no context — pass the [`DbHandle`] per call.
#[derive(Debug)]
pub struct CollectionHandle<T: NonUniqueStruct> {
    pivot: LocalId,
    _record: PhantomData<fn() -> T>,
}

// Manual impls: a derive would demand `T: Clone`/`T: Copy`, but the handle
// holds only the pivot id.
impl<T: NonUniqueStruct> Clone for CollectionHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: NonUniqueStruct> Copy for CollectionHandle<T> {}

impl<T: NonUniqueStruct> CollectionHandle<T> {
    /// The handle for the collection whose `Pivot` record sits at `pivot`.
    #[must_use]
    pub const fn at(pivot: LocalId) -> Self {
        Self {
            pivot,
            _record: PhantomData,
        }
    }

    /// The `LocalId` of this collection's `Pivot` record.
    #[must_use]
    pub const fn pivot(&self) -> LocalId {
        self.pivot
    }

    /// Insert `value`, returning its stable identity [`Id`].
    ///
    /// # Errors
    /// A backend/transport failure, or a stale/foreign pivot.
    pub async fn insert<D: DbHandle>(
        &self,
        db: &D,
        value: &T,
    ) -> Result<Id, D::Error> {
        db.insert(self.pivot, value).await
    }

    /// Fetch the record at `id`. `None` = no such record; a removed record
    /// still resolves (history stays navigable).
    ///
    /// # Errors
    /// A backend/transport failure or a decode fault.
    pub async fn get<D: DbHandle>(
        &self,
        db: &D,
        id: Id,
    ) -> Result<Option<T>, D::Error> {
        db.get_record(self.pivot, id).await
    }

    /// Update the record at `id` to `value` — same identity, new bytes; the
    /// superseded version is archived, changed secondary indexes re-key.
    ///
    /// # Errors
    /// A backend/transport failure, or a missing record.
    pub async fn save<D: DbHandle>(
        &self,
        db: &D,
        id: Id,
        value: &T,
    ) -> Result<(), D::Error> {
        db.update(self.pivot, id, value).await
    }

    /// Move the record at `id` to the dead tree; returns whether it was
    /// living. Bytes are kept.
    ///
    /// # Errors
    /// A backend/transport failure.
    pub async fn remove<D: DbHandle>(
        &self,
        db: &D,
        id: Id,
    ) -> Result<bool, D::Error> {
        db.remove::<T>(self.pivot, id).await
    }

    // The stream-returning reads capture `db`'s lifetime but deliberately
    // NOT the receiver's (`use<..>`): the handle is `Copy` and only its
    // `pivot` value feeds the stream, so `T::collection(p).all(db)` on a
    // temporary handle must work.

    /// Stream every living record in insertion (`CREATED_AT`) order.
    pub fn all<'d, D: DbHandle>(
        &self,
        db: &'d D,
    ) -> impl Stream<Item = Result<T, D::Error>> + use<'d, D, T>
    where
        T: 'static,
    {
        db.all(self.pivot)
    }

    /// Stream the living records secondary index `index` selects under
    /// `bound`, ordered by the indexed field. The generated `by_<field>`
    /// wrappers call this with the field's exact encoding.
    pub fn search_by<'d, D: DbHandle>(
        &self,
        db: &'d D,
        index: usize,
        bound: Bound,
    ) -> impl Stream<Item = Result<T, D::Error>> + use<'d, D, T>
    where
        T: 'static,
    {
        db.search_by(self.pivot, index, bound)
    }

    /// Stream the record at `id`'s versions **newest-first** along the
    /// modification chain. Saving never destroys old bytes — this walks them.
    pub fn history<'d, D: DbHandle>(
        &self,
        db: &'d D,
        id: Id,
    ) -> impl Stream<Item = Result<(Metadata, T), D::Error>> + use<'d, D, T>
    where
        T: 'static,
    {
        db.record_history(self.pivot, id)
    }
}
