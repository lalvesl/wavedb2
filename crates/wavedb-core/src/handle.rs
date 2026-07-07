//! [`DbHandle`] — the one execution-context seam typed generated code runs
//! over, and [`LocalHandle`], its `Store`-backed implementation.
//!
//! The `#[wavedb]` macro emits `T::get(&db)` / `value.save(&db)` /
//! `T::collection(&db, pivot)` methods generic over this trait, so the same
//! call sites resolve against every context: a [`LocalHandle`] driving a
//! [`Store`] directly (engine tests, in-process embedding), the client `Db`
//! sending command frames, and the node-side `ServerDb` a `#[server]` body
//! runs against.
//!
//! Two deliberate signature choices:
//!
//! - **`type Error: From<Error>`** — the client's error is richer than
//!   core's (node refusals, transport faults), so each context brings its
//!   own; core errors convert in.
//! - **Walk-shaped ops return `impl Stream`** even where an implementation
//!   buffers today (the M4 client collects a `Reply::Values` and wraps it in
//!   an iterator stream) — when streaming frames land, only implementations
//!   change, never the generated call sites.

use futures::{Stream, TryStreamExt};

use crate::collection::Collection;
use crate::error::Error;
use crate::id::Id;
use crate::index::Bound;
use crate::local_id::LocalId;
use crate::metadata::Metadata;
use crate::record;
use crate::store::Store;
use crate::traits::{NonUniqueStruct, UniqueStruct};
use crate::u48::U48;

/// An execution context bound to one tenant: somewhere typed operations can
/// run — locally against a [`Store`], or remotely over a transport.
///
/// The tenant is bound **once, in the handle** — the partition key is
/// structural, never restated per call. Collection ops address their
/// collection by the `Pivot` record's [`LocalId`] (the generated typed
/// wrappers pass it from a `{Name}PivotId`).
pub trait DbHandle: Sized {
    /// This context's error. Core faults convert in; a context may add its
    /// own layers (node refusal, transport) on top.
    type Error: From<Error>;

    /// The tenant this handle is bound to.
    fn tenant(&self) -> U48;

    /// The same context scoped to a different tenant — the server-side
    /// cross-tenant seam (a `register`-style function bootstrapping a new
    /// tenant's records). Not a privilege escalation by itself: enforcement
    /// is the node's job (M8), not the handle's.
    #[must_use]
    fn as_tenant(&self, tenant: U48) -> Self;

    /// Fetch this tenant's `Unique` record from its anchor. `None` = never
    /// saved.
    ///
    /// # Errors
    /// A backend/transport failure or a decode fault.
    async fn get_unique<T: UniqueStruct>(
        &self,
    ) -> Result<Option<T>, Self::Error>;

    /// Save (insert-or-overwrite) this tenant's `Unique` record at its
    /// anchor. Save **is** the upsert; the superseded version is archived on
    /// the modification chain.
    ///
    /// # Errors
    /// A backend/transport failure.
    async fn save_unique<T: UniqueStruct>(
        &self,
        value: &T,
    ) -> Result<(), Self::Error>;

    /// Stream this tenant's `Unique` record versions **newest-first** (the
    /// live record, then each archive along the modification chain). Empty
    /// when never saved.
    ///
    /// The `'static` bound on the walk-shaped ops is free: `WaveWire` values
    /// are always owned (decode never borrows), so every `#[wavedb]` type is
    /// `'static` — and it unties the yielded items from the handle borrow.
    fn unique_history<T: UniqueStruct + 'static>(
        &self,
    ) -> impl Stream<Item = Result<(Metadata, T), Self::Error>>;

    /// Create a new, empty collection of `T` under this tenant — explicit,
    /// never automatic. The caller stores the returned root (via the typed
    /// `{Name}PivotId`) in an owning record.
    ///
    /// # Errors
    /// A backend/transport failure.
    async fn create_pivot<T: NonUniqueStruct>(
        &self,
    ) -> Result<LocalId, Self::Error>;

    /// Insert `value` into the collection at `pivot`, returning its stable
    /// identity [`Id`] (the anchor references point at — it never changes).
    ///
    /// # Errors
    /// A backend/transport failure, or a stale/foreign `pivot`.
    async fn insert<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        value: &T,
    ) -> Result<Id, Self::Error>;

    /// Fetch the record at `id`. `None` = no such record. A removed record
    /// still resolves (history stays navigable); `pivot` scopes the typed
    /// wrapper, remote contexts may not need it.
    ///
    /// # Errors
    /// A backend/transport failure or a decode fault (including an `id`
    /// resolving to a different type's record).
    async fn get_record<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        id: Id,
    ) -> Result<Option<T>, Self::Error>;

    /// Update the record at `id` to `value` — same identity, new bytes; the
    /// superseded version is archived, changed secondary indexes re-key.
    ///
    /// # Errors
    /// A backend/transport failure, or a missing record.
    async fn update<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        id: Id,
        value: &T,
    ) -> Result<(), Self::Error>;

    /// Move the record at `id` from the living set to the dead tree; returns
    /// whether it was living. Bytes are kept — nothing is erased.
    ///
    /// # Errors
    /// A backend/transport failure.
    async fn remove<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        id: Id,
    ) -> Result<bool, Self::Error>;

    /// Stream every living record of the collection at `pivot`, in insertion
    /// (`CREATED_AT`) order.
    fn all<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
    ) -> impl Stream<Item = Result<T, Self::Error>>;

    /// Stream the living records secondary index `index` selects under
    /// `bound`, ordered by the indexed field. The generated `by_<field>`
    /// wrappers call this with the field's exact
    /// [`IndexKey`](crate::index::IndexKey) encoding.
    fn search_by<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
        index: usize,
        bound: Bound,
    ) -> impl Stream<Item = Result<T, Self::Error>>;

    /// Stream the record at `id`'s versions **newest-first** along the
    /// modification chain (the live version, then each archive).
    fn record_history<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
        id: Id,
    ) -> impl Stream<Item = Result<(Metadata, T), Self::Error>>;
}

/// A [`DbHandle`] over a borrowed [`Store`] — the engine-local context.
///
/// Core/storage tests and in-process embeddings drive typed code through it;
/// every op is a direct [`Collection`] / record call.
#[derive(Debug)]
pub struct LocalHandle<'a, S> {
    store: &'a S,
    tenant: U48,
}

// Manual impls: a derive would demand `S: Clone`, but only the reference is
// copied.
impl<S> Clone for LocalHandle<'_, S> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<S> Copy for LocalHandle<'_, S> {}

impl<'a, S: Store> LocalHandle<'a, S> {
    /// A local context over `store`, bound to `tenant`.
    #[must_use]
    pub const fn new(store: &'a S, tenant: U48) -> Self {
        Self { store, tenant }
    }

    /// The backing store.
    #[must_use]
    pub const fn store(&self) -> &'a S {
        self.store
    }

    /// This type's collection engine handle at `pivot`.
    fn col<T: NonUniqueStruct>(&self, pivot: LocalId) -> Collection<T> {
        Collection::at(pivot, self.tenant)
    }
}

impl<S: Store> DbHandle for LocalHandle<'_, S> {
    type Error = Error;

    fn tenant(&self) -> U48 {
        self.tenant
    }

    fn as_tenant(&self, tenant: U48) -> Self {
        Self {
            store: self.store,
            tenant,
        }
    }

    async fn get_unique<T: UniqueStruct>(&self) -> Result<Option<T>, Error> {
        record::get_unique(self.store, self.tenant).await
    }

    async fn save_unique<T: UniqueStruct>(
        &self,
        value: &T,
    ) -> Result<(), Error> {
        record::save_unique(self.store, self.tenant, value).await
    }

    fn unique_history<T: UniqueStruct + 'static>(
        &self,
    ) -> impl Stream<Item = Result<(Metadata, T), Error>> {
        record::unique_history(self.store, self.tenant)
    }

    async fn create_pivot<T: NonUniqueStruct>(&self) -> Result<LocalId, Error> {
        Collection::<T>::create(self.store, self.tenant).await
    }

    async fn insert<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        value: &T,
    ) -> Result<Id, Error> {
        self.col::<T>(pivot).insert(self.store, value).await
    }

    async fn get_record<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        id: Id,
    ) -> Result<Option<T>, Error> {
        self.col::<T>(pivot).get(self.store, id).await
    }

    async fn update<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        id: Id,
        value: &T,
    ) -> Result<(), Error> {
        self.col::<T>(pivot).save(self.store, id, value).await
    }

    async fn remove<T: NonUniqueStruct>(
        &self,
        pivot: LocalId,
        id: Id,
    ) -> Result<bool, Error> {
        self.col::<T>(pivot).remove(self.store, id).await
    }

    fn all<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
    ) -> impl Stream<Item = Result<T, Error>> {
        self.col::<T>(pivot)
            .all(self.store)
            .map_ok(|(_, value)| value)
    }

    fn search_by<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
        index: usize,
        bound: Bound,
    ) -> impl Stream<Item = Result<T, Error>> {
        self.col::<T>(pivot)
            .search_by(self.store, index, bound)
            .map_ok(|(_, value)| value)
    }

    fn record_history<T: NonUniqueStruct + 'static>(
        &self,
        pivot: LocalId,
        id: Id,
    ) -> impl Stream<Item = Result<(Metadata, T), Error>> {
        self.col::<T>(pivot).history(self.store, id)
    }
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use futures::executor::block_on;

    use super::{DbHandle, LocalHandle};
    use crate::index::mem_store::MemStore;
    use crate::index::{IndexKey, Pivot};
    use crate::local_id::LocalId;
    use crate::permission::PermissionRef;
    use crate::traits::{NonUniqueStruct, Shape, UniqueStruct, WaveDbStruct};
    use crate::u48::U48;
    use crate::wire::WaveWire;

    // Hand-rolled fixtures — exactly what `#[wavedb]` generates (core can't
    // use the proc-macro; it lives downstream).
    #[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
    struct Settings {
        volume: u64,
    }
    impl WaveDbStruct for Settings {
        const STRUCT_HASH: u64 = 0x5E77_1001;
        const SHAPE: Shape = Shape::Unique;
        type PivotId = ();
    }
    impl UniqueStruct for Settings {}

    #[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
    struct Note {
        label: String,
        n: u64,
    }
    impl WaveDbStruct for Note {
        const STRUCT_HASH: u64 = 0x2077_1001;
        const SHAPE: Shape = Shape::NonUnique;
        type PivotId = ();
    }
    impl NonUniqueStruct for Note {
        type Pivot = NotePivot;
        const NUM_SECONDARIES: usize = 1;
        fn secondary_key(&self, index: usize) -> Vec<u8> {
            match index {
                0 => self.label.key_bytes(),
                _ => Vec::new(),
            }
        }
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, WaveWire)]
    struct NotePivot {
        current: LocalId,
        dead: LocalId,
        secondaries: [LocalId; 1],
        permission: Option<PermissionRef>,
    }
    impl Pivot for NotePivot {
        const STRUCT_HASH: u64 = 0x2077_1002;
        fn current(&self) -> LocalId {
            self.current
        }
        fn dead(&self) -> LocalId {
            self.dead
        }
        fn secondaries(&self) -> &[LocalId] {
            &self.secondaries
        }
        fn permission(&self) -> Option<&PermissionRef> {
            self.permission.as_ref()
        }
        fn replace_roots(
            &self,
            current: LocalId,
            dead: LocalId,
            secondaries: &[LocalId],
        ) -> Self {
            let mut s = self.secondaries;
            s.copy_from_slice(secondaries);
            Self {
                current,
                dead,
                secondaries: s,
                permission: self.permission.clone(),
            }
        }
    }

    fn tenant() -> U48 {
        U48::from(7u32)
    }

    #[test]
    fn unique_roundtrip_and_history_through_the_handle() {
        block_on(async {
            let store = MemStore::default();
            let db = LocalHandle::new(&store, tenant());

            assert_eq!(db.get_unique::<Settings>().await.unwrap(), None);
            db.save_unique(&Settings { volume: 3 }).await.unwrap();
            db.save_unique(&Settings { volume: 7 }).await.unwrap();
            assert_eq!(
                db.get_unique::<Settings>().await.unwrap(),
                Some(Settings { volume: 7 })
            );

            let versions: Vec<(crate::metadata::Metadata, Settings)> =
                db.unique_history().try_collect().await.unwrap();
            assert_eq!(
                versions.iter().map(|(_, s)| s.volume).collect::<Vec<_>>(),
                vec![7, 3],
                "history walks newest-first"
            );

            // Another tenant's context sees nothing — same store, own space.
            let other = db.as_tenant(U48::from(8u32));
            assert_eq!(other.get_unique::<Settings>().await.unwrap(), None);
            assert_eq!(other.tenant(), U48::from(8u32));
        });
    }

    #[test]
    fn collection_lifecycle_through_the_handle() {
        block_on(async {
            let store = MemStore::default();
            let db = LocalHandle::new(&store, tenant());
            let pivot = db.create_pivot::<Note>().await.unwrap();

            let note = |label: &str, n| Note {
                label: label.into(),
                n,
            };
            let a = db.insert(pivot, &note("red", 1)).await.unwrap();
            let b = db.insert(pivot, &note("blue", 2)).await.unwrap();

            let walked: Vec<Note> =
                db.all::<Note>(pivot).try_collect().await.unwrap();
            assert_eq!(
                walked.iter().map(|v| v.n).collect::<Vec<_>>(),
                vec![1, 2],
                "walk is insertion-ordered and yields values"
            );

            db.update(pivot, b, &note("blue", 22)).await.unwrap();
            assert_eq!(
                db.get_record::<Note>(pivot, b).await.unwrap(),
                Some(note("blue", 22))
            );

            // Secondary lookup through the handle: index 0 = `label`.
            let reds: Vec<Note> = db
                .search_by::<Note>(
                    pivot,
                    0,
                    crate::index::Bound::Exact("red".key_bytes()),
                )
                .try_collect()
                .await
                .unwrap();
            assert_eq!(reds, vec![note("red", 1)]);

            assert!(db.remove::<Note>(pivot, a).await.unwrap());
            assert!(!db.remove::<Note>(pivot, a).await.unwrap());
            let after: Vec<Note> =
                db.all::<Note>(pivot).try_collect().await.unwrap();
            assert_eq!(after, vec![note("blue", 22)]);
            assert_eq!(
                db.get_record::<Note>(pivot, a).await.unwrap(),
                Some(note("red", 1)),
                "removed record bytes survive (history)"
            );
        });
    }
}
