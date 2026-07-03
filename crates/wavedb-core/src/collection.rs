//! [`Collection`] — the typed, `Store`-generic engine behind a NonUnique type's
//! generated collection API, plus the Unique-anchor helpers.
//!
//! `#[wavedb(NonUnique)]` emits `Todo::collection(pivot_id, tenant)` /
//! `Todo::create_pivot(store, tenant)` wrappers over this type, so application
//! code drives inserts, removals, and walks through the collection — never a
//! raw `BpTree`, which stays an index-layer internal:
//!
//! ```text
//! let todos = Todo::create_pivot(&store, tenant).await?;   // once, explicit
//! let col   = Todo::collection(todos, tenant);              // cheap handle
//! let id    = col.insert(&store, &Todo { .. }).await?;      // one atomic batch
//! col.remove(&store, id).await?;                            // current → dead
//! col.all(&store)                                           // Stream<Result<Todo>>
//! ```
//!
//! ## Record envelope
//!
//! Every stored value is `[STRUCT_HASH (8 B LE)][WaveWire bytes]` — storage
//! backends route by those first 8 bytes, and decode verifies them, so a stale
//! or foreign `Id` can't silently decode as the wrong type.
//!
//! ## Atomicity
//!
//! Each mutating op commits **one** [`Store::apply`] batch: the record write,
//! every touched B+tree node (via the tree's `plan_*` planners), and — when a
//! root moved — the rewritten `Pivot` record. A crash replays the whole batch
//! or none of it.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::{Stream, TryStreamExt};

use crate::error::{Error, Result};
use crate::id::Id;
use crate::index::{Bound, BpTree, Pivot};
use crate::local_id::LocalId;
use crate::store::{Store, Write};
use crate::traits::NonUniqueStruct;
use crate::u48::U48;
use crate::wire::{from_wire, to_wire};

/// Bytes before the wire body: the `STRUCT_HASH` head.
const ENVELOPE_PREFIX: usize = 8;

/// Process-wide counter salting minted record ids, so two records minted in
/// the same nanosecond still get distinct ids.
static RECORD_SALT: AtomicU64 = AtomicU64::new(0);

/// Serialise a value as a stored record: `[hash (8 B LE)][WaveWire bytes]`.
fn encode_envelope<V: crate::wire::WaveWire>(hash: u64, value: &V) -> Vec<u8> {
    let mut out = hash.to_le_bytes().to_vec();
    out.extend_from_slice(&to_wire(value));
    out
}

/// Decode a stored record, verifying its `STRUCT_HASH` head first.
///
/// # Errors
/// [`Error::UnknownStructHash`] if the head is not `hash` (or the value is
/// shorter than the head); [`Error::Wire`] if the body fails to decode.
fn decode_envelope<V: crate::wire::WaveWire>(
    hash: u64,
    bytes: &[u8],
) -> Result<V> {
    let head: [u8; ENVELOPE_PREFIX] = bytes
        .get(..ENVELOPE_PREFIX)
        .and_then(|s| s.try_into().ok())
        .ok_or(Error::UnknownStructHash(0))?;
    let got = u64::from_le_bytes(head);
    if got != hash {
        return Err(Error::UnknownStructHash(got));
    }
    Ok(from_wire::<V>(&bytes[ENVELOPE_PREFIX..])?)
}

/// Mint a fresh timestamp-keyed id under `tenant`: `KEY = CREATED_AT` (nanos),
/// `FLAG = 0` (the record namespace), and a per-process counter salt so ids
/// minted in the same nanosecond stay distinct.
fn mint_timestamped_id(tenant: U48) -> Id {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let salt = (RECORD_SALT.fetch_add(1, Ordering::Relaxed) & 0x7FFF) as u16;
    Id::new(nanos, tenant, false, salt)
}

// ---- Unique anchors -----------------------------------------------------------

/// Fetch a `Unique` record from its anchor (`KEY = STRUCT_HASH`, `FLAG = 1`,
/// `SALT = 0`) under `tenant`. `None` = never saved.
///
/// # Errors
/// Propagates a [`Store`] failure or a decode fault.
pub async fn get_unique<T, S>(store: &S, tenant: U48) -> Result<Option<T>>
where
    T: crate::traits::WaveDbStruct,
    S: Store,
{
    let anchor = Id::new(T::STRUCT_HASH, tenant, true, 0);
    match store.get_of(T::STRUCT_HASH, anchor).await? {
        Some(bytes) => Ok(Some(decode_envelope(T::STRUCT_HASH, &bytes)?)),
        None => Ok(None),
    }
}

/// Save (insert-or-overwrite) a `Unique` record at its anchor under `tenant`.
/// `save` **is** the upsert — `Unique` types have no separate create.
///
/// # Errors
/// Propagates a [`Store`] failure.
pub async fn save_unique<T, S>(store: &S, tenant: U48, value: &T) -> Result<()>
where
    T: crate::traits::WaveDbStruct,
    S: Store,
{
    let anchor = Id::new(T::STRUCT_HASH, tenant, true, 0);
    let record = encode_envelope(T::STRUCT_HASH, value);
    store.apply(&[Write::Put(anchor, record)]).await
}

// ---- Collection ---------------------------------------------------------------

/// The typed handle into one NonUnique collection.
///
/// Holds a `PivotId` plus the tenant, nothing else — cheap to construct per
/// call. Every op loads the `Pivot` record fresh, so concurrent root moves are
/// always observed.
#[derive(Debug)]
pub struct Collection<T: NonUniqueStruct> {
    pivot: LocalId,
    tenant: U48,
    leaf_cap: usize,
    internal_cap: usize,
    _record: PhantomData<fn() -> T>,
}

// Manual impls: a derive would demand `T: Clone`/`T: Copy`, but the handle
// holds only ids (the `PhantomData` is `fn() -> T`, unconditionally `Copy`).
impl<T: NonUniqueStruct> Clone for Collection<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: NonUniqueStruct> Copy for Collection<T> {}

impl<T: NonUniqueStruct> Collection<T> {
    /// The handle for the collection referenced by `pivot` under `tenant`.
    #[must_use]
    pub const fn at(pivot: LocalId, tenant: U48) -> Self {
        Self {
            pivot,
            tenant,
            leaf_cap: crate::index::DEFAULT_LEAF_CAP,
            internal_cap: crate::index::DEFAULT_INTERNAL_CAP,
            _record: PhantomData,
        }
    }

    /// Override the B+tree node capacities (small caps make deep trees cheap
    /// to build in tests; production uses the defaults). Use one configuration
    /// for a collection's whole lifetime.
    #[must_use]
    pub const fn with_caps(mut self, leaf: usize, internal: usize) -> Self {
        self.leaf_cap = leaf;
        self.internal_cap = internal;
        self
    }

    /// A tree handle at `root` with this collection's capacities.
    fn tree(&self, root: LocalId) -> BpTree {
        BpTree::at(root, self.tenant)
            .with_caps(self.leaf_cap, self.internal_cap)
    }

    /// Create a new, empty collection under `tenant`: two empty B+trees
    /// (`current` + `dead`) and the `Pivot` record pointing at them, committed
    /// in one atomic batch. Returns the pivot's `LocalId` — the caller stores
    /// it (via the generated `{Name}PivotId`) in an owning record.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure.
    pub async fn create<S: Store>(store: &S, tenant: U48) -> Result<LocalId> {
        let (current, current_write) = BpTree::plan_create(tenant);
        let (dead, dead_write) = BpTree::plan_create(tenant);
        let pivot_record =
            T::Pivot::default().replace_roots(current.root(), dead.root());
        let pivot_id = mint_timestamped_id(tenant);
        let pivot_write = Write::Put(
            pivot_id,
            encode_envelope(T::Pivot::STRUCT_HASH, &pivot_record),
        );
        store
            .apply(&[current_write, dead_write, pivot_write])
            .await?;
        Ok(LocalId::from_id(pivot_id))
    }

    /// The `LocalId` of this collection's `Pivot` record.
    #[must_use]
    pub const fn pivot(&self) -> LocalId {
        self.pivot
    }

    /// Load and decode the `Pivot` record.
    async fn load_pivot<S: Store>(&self, store: &S) -> Result<T::Pivot> {
        let bytes = store
            .get_of(T::Pivot::STRUCT_HASH, self.pivot.to_id(self.tenant))
            .await?
            .ok_or(Error::PivotMissing(self.pivot))?;
        decode_envelope(T::Pivot::STRUCT_HASH, &bytes)
    }

    /// A `Put` rewriting the `Pivot` record with moved roots.
    fn pivot_rewrite(&self, pivot: &T::Pivot) -> Write {
        Write::Put(
            self.pivot.to_id(self.tenant),
            encode_envelope(T::Pivot::STRUCT_HASH, pivot),
        )
    }

    /// Insert `value` as a new record: mints its timestamp-keyed [`Id`] (the
    /// stable identity for the record's whole life), writes the record, indexes
    /// it in `current`, and rewrites the `Pivot` if the root moved — one atomic
    /// batch. Returns the minted `Id`.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure, [`Error::PivotMissing`] on a stale
    /// handle, or a decode fault on a corrupt pivot.
    pub async fn insert<S: Store>(&self, store: &S, value: &T) -> Result<Id> {
        let pivot = self.load_pivot(store).await?;
        let mut current = self.tree(pivot.current());

        let id = mint_timestamped_id(self.tenant);
        let mut batch =
            vec![Write::Put(id, encode_envelope(T::STRUCT_HASH, value))];
        batch.extend(current.plan_insert(store, id).await?);
        if current.root() != pivot.current() {
            let moved = pivot.replace_roots(current.root(), pivot.dead());
            batch.push(self.pivot_rewrite(&moved));
        }
        store.apply(&batch).await?;
        Ok(id)
    }

    /// Remove the record at `id` from the living set: de-indexes it from
    /// `current` and indexes it in `dead` — one atomic batch. The record
    /// **bytes stay** (history stays navigable); only `remove` ever writes the
    /// `dead` tree. Returns whether the record was in `current`.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure, [`Error::PivotMissing`] on a stale
    /// handle, or a decode fault on a corrupt pivot.
    pub async fn remove<S: Store>(&self, store: &S, id: Id) -> Result<bool> {
        let pivot = self.load_pivot(store).await?;
        let mut current = self.tree(pivot.current());
        let mut dead = self.tree(pivot.dead());

        let Some(mut batch) = current.plan_remove(store, id).await? else {
            return Ok(false);
        };
        batch.extend(dead.plan_insert(store, id).await?);
        if current.root() != pivot.current() || dead.root() != pivot.dead() {
            let moved = pivot.replace_roots(current.root(), dead.root());
            batch.push(self.pivot_rewrite(&moved));
        }
        store.apply(&batch).await?;
        Ok(true)
    }

    /// Fetch and decode the record at `id`. `None` = no such record. Resolves
    /// by direct address — a removed (dead-indexed) record still resolves,
    /// which is what keeps history navigable.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure or a decode fault (including an `id`
    /// that resolves to a different type's record).
    pub async fn get<S: Store>(&self, store: &S, id: Id) -> Result<Option<T>> {
        match store.get_of(T::STRUCT_HASH, id).await? {
            Some(bytes) => Ok(Some(decode_envelope(T::STRUCT_HASH, &bytes)?)),
            None => Ok(None),
        }
    }

    /// Overwrite the record at `id` with `value` — the NonUnique *update*. The
    /// `Id` (and so the record's place in `current`) is unchanged, so no
    /// reindex is needed. The caller supplies an `id` it got from
    /// [`insert`](Self::insert) or a walk; writing to a never-inserted `id`
    /// stores an unindexed orphan.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure.
    pub async fn save<S: Store>(
        &self,
        store: &S,
        id: Id,
        value: &T,
    ) -> Result<()> {
        store
            .apply(&[Write::Put(id, encode_envelope(T::STRUCT_HASH, value))])
            .await
    }

    /// Stream the living records whose `CREATED_AT` falls in `bound`, in
    /// chronological order — the two-phase walk (index → `Id`s → fetch +
    /// decode) fused into one stream.
    pub fn search<'a, S: Store>(
        &self,
        store: &'a S,
        bound: Bound,
    ) -> impl Stream<Item = Result<(Id, T)>> + 'a
    where
        T: 'a,
    {
        let handle = *self;
        futures::stream::once(async move {
            let pivot = handle.load_pivot(store).await?;
            let tree = handle.tree(pivot.current());
            Ok::<_, Error>(tree.search(store, bound))
        })
        .try_flatten()
        .and_then(move |id| async move {
            let bytes = store
                .get_of(T::STRUCT_HASH, id)
                .await?
                .ok_or(Error::RecordMissing(id))?;
            Ok((id, decode_envelope::<T>(T::STRUCT_HASH, &bytes)?))
        })
    }

    /// Stream every living record in insertion (`CREATED_AT`) order.
    pub fn all<'a, S: Store>(
        &self,
        store: &'a S,
    ) -> impl Stream<Item = Result<(Id, T)>> + 'a
    where
        T: 'a,
    {
        self.search(store, Bound::All)
    }
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use futures::executor::block_on;

    use super::{Collection, get_unique, save_unique};
    use crate::error::Error;
    use crate::index::Pivot;
    use crate::index::mem_store::MemStore;
    use crate::local_id::LocalId;
    use crate::permission::PermissionRef;
    use crate::traits::{NonUniqueStruct, Shape, WaveDbStruct};
    use crate::u48::U48;
    use crate::wire::WaveWire;

    const TENANT: u32 = 9;

    // Hand-rolled fixture — core can't use the `#[wavedb]` proc-macro (it
    // lives downstream), so this is exactly what the macro generates.
    #[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
    struct Doc {
        n: u64,
    }

    impl WaveDbStruct for Doc {
        const STRUCT_HASH: u64 = 0xD0C_0001;
        const SHAPE: Shape = Shape::NonUnique;
        type PivotId = ();
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, WaveWire)]
    struct DocPivot {
        current: LocalId,
        dead: LocalId,
        permission: Option<PermissionRef>,
    }

    impl Pivot for DocPivot {
        const STRUCT_HASH: u64 = 0xD0C_0002;
        fn current(&self) -> LocalId {
            self.current
        }
        fn dead(&self) -> LocalId {
            self.dead
        }
        fn secondaries(&self) -> &[LocalId] {
            &[]
        }
        fn permission(&self) -> Option<&PermissionRef> {
            self.permission.as_ref()
        }
        fn replace_roots(&self, current: LocalId, dead: LocalId) -> Self {
            Self {
                current,
                dead,
                permission: self.permission.clone(),
            }
        }
    }

    impl NonUniqueStruct for Doc {
        type Pivot = DocPivot;
    }

    #[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
    struct Settings {
        volume: u64,
    }

    impl WaveDbStruct for Settings {
        const STRUCT_HASH: u64 = 0x5E77_0001;
        const SHAPE: Shape = Shape::Unique;
        type PivotId = ();
    }

    fn tenant() -> U48 {
        U48::from(TENANT)
    }

    #[test]
    fn unique_save_is_upsert_and_get_roundtrips() {
        block_on(async {
            let store = MemStore::default();
            assert_eq!(
                get_unique::<Settings, _>(&store, tenant()).await.unwrap(),
                None
            );
            save_unique(&store, tenant(), &Settings { volume: 3 })
                .await
                .unwrap();
            save_unique(&store, tenant(), &Settings { volume: 7 })
                .await
                .unwrap(); // overwrite, same anchor
            assert_eq!(
                get_unique::<Settings, _>(&store, tenant()).await.unwrap(),
                Some(Settings { volume: 7 })
            );
            // A different tenant sees nothing.
            assert_eq!(
                get_unique::<Settings, _>(&store, U48::from(TENANT + 1))
                    .await
                    .unwrap(),
                None
            );
        });
    }

    #[test]
    fn insert_walk_save_remove_lifecycle() {
        block_on(async {
            let store = MemStore::default();
            let pivot =
                Collection::<Doc>::create(&store, tenant()).await.unwrap();
            let col = Collection::<Doc>::at(pivot, tenant());

            let a = col.insert(&store, &Doc { n: 1 }).await.unwrap();
            let b = col.insert(&store, &Doc { n: 2 }).await.unwrap();
            let c = col.insert(&store, &Doc { n: 3 }).await.unwrap();

            let walked: Vec<(crate::id::Id, Doc)> =
                col.all(&store).try_collect().await.unwrap();
            assert_eq!(
                walked.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                vec![a, b, c],
                "walk must be insertion-ordered"
            );
            assert_eq!(walked[0].1, Doc { n: 1 });

            // Update in place: same Id, new bytes, still one walk entry.
            col.save(&store, b, &Doc { n: 22 }).await.unwrap();
            assert_eq!(col.get(&store, b).await.unwrap(), Some(Doc { n: 22 }));

            // Remove drops it from the walk but keeps the bytes (history).
            assert!(col.remove(&store, b).await.unwrap());
            assert!(!col.remove(&store, b).await.unwrap(), "already dead");
            let after: Vec<(crate::id::Id, Doc)> =
                col.all(&store).try_collect().await.unwrap();
            assert_eq!(
                after.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                vec![a, c]
            );
            assert_eq!(
                col.get(&store, b).await.unwrap(),
                Some(Doc { n: 22 }),
                "removed record bytes must survive"
            );
        });
    }

    #[test]
    fn root_moves_rewrite_the_pivot_and_survive_reopen() {
        block_on(async {
            let store = MemStore::default();
            let pivot =
                Collection::<Doc>::create(&store, tenant()).await.unwrap();
            let col = Collection::<Doc>::at(pivot, tenant()).with_caps(4, 4);

            // Enough inserts to split the current tree's root repeatedly.
            let mut ids = Vec::new();
            for n in 0..64u64 {
                ids.push(col.insert(&store, &Doc { n }).await.unwrap());
            }

            // A fresh handle (same PivotId) must see everything: the moved
            // root came from the rewritten Pivot record, not handle state.
            let reopened =
                Collection::<Doc>::at(pivot, tenant()).with_caps(4, 4);
            let walked: Vec<(crate::id::Id, Doc)> =
                reopened.all(&store).try_collect().await.unwrap();
            assert_eq!(walked.len(), 64);
            assert_eq!(
                walked.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                ids,
                "insertion order lost across root splits"
            );

            // Remove everything; merges collapse roots — pivot keeps up.
            for id in &ids {
                assert!(reopened.remove(&store, *id).await.unwrap());
            }
            let empty: Vec<(crate::id::Id, Doc)> =
                reopened.all(&store).try_collect().await.unwrap();
            assert!(empty.is_empty());
        });
    }

    #[test]
    fn stale_pivot_and_wrong_type_are_typed_errors() {
        block_on(async {
            let store = MemStore::default();
            let bogus =
                Collection::<Doc>::at(LocalId::new(42, false, 1), tenant());
            assert!(matches!(
                bogus.insert(&store, &Doc { n: 1 }).await,
                Err(Error::PivotMissing(_))
            ));

            // A Unique record read back as the wrong type is rejected by the
            // envelope's STRUCT_HASH check, not mis-decoded.
            save_unique(&store, tenant(), &Settings { volume: 1 })
                .await
                .unwrap();
            let anchor =
                crate::id::Id::new(Settings::STRUCT_HASH, tenant(), true, 0);
            let pivot =
                Collection::<Doc>::create(&store, tenant()).await.unwrap();
            let col = Collection::<Doc>::at(pivot, tenant());
            assert!(matches!(
                col.get(&store, anchor).await,
                Err(Error::UnknownStructHash(h)) if h == Settings::STRUCT_HASH
            ));
        });
    }
}
