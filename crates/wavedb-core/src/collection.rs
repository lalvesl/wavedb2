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
//! ## Record envelope & history
//!
//! Every stored value starts `[STRUCT_HASH (8 B LE)]` (backends route by it,
//! decode verifies it); user records additionally carry their [`Metadata`]
//! (see [`crate::record`]). Saving never destroys old bytes — `save` archives
//! the superseded version and links the modification chain;
//! [`history`](Collection::history) walks it newest-first.
//!
//! ## Atomicity
//!
//! Each mutating op commits **one** [`Store::apply`] batch: the record write
//! (plus its archived predecessor and chain relinks on a save), every touched
//! B+tree node (via the tree's `plan_*` planners), and — when a root moved —
//! the rewritten `Pivot` record. A crash replays the whole batch or none of
//! it.

use std::marker::PhantomData;

use futures::{Stream, TryStreamExt};

use crate::error::{Error, Result};
use crate::id::Id;
use crate::index::{Bound, BpTree, Pivot, SecKey};
use crate::local_id::LocalId;
use crate::metadata::Metadata;
use crate::record::{
    decode_envelope, decode_record, encode_envelope, history_stream,
    mint_timestamped_id,
};
use crate::store::{Store, Write};
use crate::traits::NonUniqueStruct;
use crate::u48::U48;

// The `#[wavedb]` macro reaches the Unique-anchor ops through this module's
// path; the implementation lives with the envelope in [`crate::record`].
pub use crate::record::{get_unique, save_unique, unique_history};

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

    /// The tenant this handle is scoped to.
    pub(crate) const fn tenant(&self) -> U48 {
        self.tenant
    }

    /// A tree handle at `root` with this collection's capacities.
    pub(crate) fn tree(&self, root: LocalId) -> BpTree {
        BpTree::at(root, self.tenant)
            .with_caps(self.leaf_cap, self.internal_cap)
    }

    /// A secondary-index tree handle at `root` with the same capacities.
    pub(crate) fn sec_tree(&self, root: LocalId) -> BpTree<SecKey> {
        BpTree::at(root, self.tenant)
            .with_caps(self.leaf_cap, self.internal_cap)
    }

    /// The secondary-index trees this pivot declares, in declaration order.
    pub(crate) fn sec_trees(&self, pivot: &T::Pivot) -> Vec<BpTree<SecKey>> {
        pivot
            .secondaries()
            .iter()
            .map(|root| self.sec_tree(*root))
            .collect()
    }

    /// Secondary index `i`'s key for `value` stored at `id`.
    pub(crate) fn sec_key(value: &T, i: usize, id: Id) -> SecKey {
        SecKey {
            field: value.secondary_key(i),
            rec: LocalId::from_id(id),
        }
    }

    /// Create a new, empty collection under `tenant`: the `current` + `dead`
    /// B+trees, one secondary tree per `#[wavedb::pivot(...)]`, and the
    /// `Pivot` record pointing at them all, committed in one atomic batch.
    /// Returns the pivot's `LocalId` — the caller stores it (via the generated
    /// `{Name}PivotId`) in an owning record.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure.
    pub async fn create<S: Store>(store: &S, tenant: U48) -> Result<LocalId> {
        let (current, current_write) = BpTree::<LocalId>::plan_create(tenant);
        let (dead, dead_write) = BpTree::<LocalId>::plan_create(tenant);
        let mut batch = vec![current_write, dead_write];
        let mut sec_roots = Vec::with_capacity(T::NUM_SECONDARIES);
        for _ in 0..T::NUM_SECONDARIES {
            let (tree, write) = BpTree::<SecKey>::plan_create(tenant);
            sec_roots.push(tree.root());
            batch.push(write);
        }
        let pivot_record = T::Pivot::default().replace_roots(
            current.root(),
            dead.root(),
            &sec_roots,
        );
        let pivot_id = mint_timestamped_id(tenant);
        batch.push(Write::Put(
            pivot_id,
            encode_envelope(T::Pivot::STRUCT_HASH, &pivot_record),
        ));
        store.apply(&batch).await?;
        Ok(LocalId::from_id(pivot_id))
    }

    /// The `LocalId` of this collection's `Pivot` record.
    #[must_use]
    pub const fn pivot(&self) -> LocalId {
        self.pivot
    }

    /// Load and decode the `Pivot` record.
    pub(crate) async fn load_pivot<S: Store>(
        &self,
        store: &S,
    ) -> Result<T::Pivot> {
        let bytes = store
            .get_of(T::Pivot::STRUCT_HASH, self.pivot.to_id(self.tenant))
            .await?
            .ok_or(Error::PivotMissing(self.pivot))?;
        decode_envelope(T::Pivot::STRUCT_HASH, &bytes)
    }

    /// A `Put` rewriting the `Pivot` record with moved roots.
    pub(crate) fn pivot_rewrite(&self, pivot: &T::Pivot) -> Write {
        Write::Put(
            self.pivot.to_id(self.tenant),
            encode_envelope(T::Pivot::STRUCT_HASH, pivot),
        )
    }

    /// Load and decode the record at `id` (metadata + body), failing if it is
    /// gone.
    pub(crate) async fn load_record<S: Store>(
        &self,
        store: &S,
        id: Id,
    ) -> Result<(Metadata, T)> {
        let bytes = store
            .get_of(T::STRUCT_HASH, id)
            .await?
            .ok_or(Error::RecordMissing(id))?;
        decode_record(T::STRUCT_HASH, &bytes)
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
            Some(bytes) => Ok(Some(decode_record(T::STRUCT_HASH, &bytes)?.1)),
            None => Ok(None),
        }
    }

    /// Stream the record at `id`'s versions **newest-first**: the live
    /// version, then each archived one along the modification chain that
    /// [`save`](Self::save) maintains. Saving never destroys old bytes — this
    /// is the walk over them.
    // The read methods take `self` by value (the handle is `Copy`): they
    // return streams, and a borrowed receiver would tie the opaque type to a
    // temporary when called as `T::collection(..).all(store)`.
    pub fn history<'a, S: Store>(
        self,
        store: &'a S,
        id: Id,
    ) -> impl Stream<Item = Result<(Metadata, T)>> + 'a
    where
        T: 'a,
    {
        history_stream::<T, S>(store, T::STRUCT_HASH, id, self.tenant)
    }

    /// Stream the living records secondary index `index` selects under
    /// `bound` (a bound over the index's [`IndexKey`](crate::index::IndexKey)
    /// encoding — `Exact` for one value, `Prefix`/`Range` for scans), resolved
    /// two-phase like [`search`](Self::search). Ordered by the indexed field.
    /// The generated `by_<field>` wrappers call this with the field's exact
    /// encoding.
    pub fn search_by<'a, S: Store>(
        self,
        store: &'a S,
        index: usize,
        bound: Bound,
    ) -> impl Stream<Item = Result<(Id, T)>> + 'a
    where
        T: 'a,
    {
        let handle = self;
        futures::stream::once(async move {
            let pivot = handle.load_pivot(store).await?;
            let root = *pivot
                .secondaries()
                .get(index)
                .ok_or(Error::SecondaryIndexOutOfRange(index))?;
            Ok::<_, Error>(handle.sec_tree(root).search(store, bound))
        })
        .try_flatten()
        .and_then(move |id| async move {
            let bytes = store
                .get_of(T::STRUCT_HASH, id)
                .await?
                .ok_or(Error::RecordMissing(id))?;
            Ok((id, decode_record::<T>(T::STRUCT_HASH, &bytes)?.1))
        })
    }

    /// Stream the living records whose `CREATED_AT` falls in `bound`, in
    /// chronological order — the two-phase walk (index → `Id`s → fetch +
    /// decode) fused into one stream.
    pub fn search<'a, S: Store>(
        self,
        store: &'a S,
        bound: Bound,
    ) -> impl Stream<Item = Result<(Id, T)>> + 'a
    where
        T: 'a,
    {
        let handle = self;
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
            Ok((id, decode_record::<T>(T::STRUCT_HASH, &bytes)?.1))
        })
    }

    /// Stream every living record in insertion (`CREATED_AT`) order.
    pub fn all<'a, S: Store>(
        self,
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
        fn replace_roots(
            &self,
            current: LocalId,
            dead: LocalId,
            _secondaries: &[LocalId],
        ) -> Self {
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

    // A record with one secondary index (on `label`) — exactly the shape the
    // macro generates for `#[wavedb::pivot(label)]`.
    #[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
    struct Tagged {
        label: String,
        n: u64,
    }

    impl WaveDbStruct for Tagged {
        const STRUCT_HASH: u64 = 0x7A6_0001;
        const SHAPE: Shape = Shape::NonUnique;
        type PivotId = ();
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq, WaveWire)]
    struct TaggedPivot {
        current: LocalId,
        dead: LocalId,
        secondaries: [LocalId; 1],
        permission: Option<PermissionRef>,
    }

    impl Pivot for TaggedPivot {
        const STRUCT_HASH: u64 = 0x7A6_0002;
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

    impl NonUniqueStruct for Tagged {
        type Pivot = TaggedPivot;
        const NUM_SECONDARIES: usize = 1;
        fn secondary_key(&self, index: usize) -> Vec<u8> {
            match index {
                0 => crate::index::IndexKey::key_bytes(&self.label),
                _ => Vec::new(),
            }
        }
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

    // Saving never destroys old bytes: each save archives the superseded
    // version and links the chain; `history` walks it newest-first, and the
    // forward pointers land where the design says.
    #[test]
    fn save_archives_versions_and_history_walks() {
        block_on(async {
            let store = MemStore::default();
            let pivot =
                Collection::<Doc>::create(&store, tenant()).await.unwrap();
            let col = Collection::<Doc>::at(pivot, tenant());

            let id = col.insert(&store, &Doc { n: 1 }).await.unwrap();
            col.save(&store, id, &Doc { n: 2 }).await.unwrap();
            col.save(&store, id, &Doc { n: 3 }).await.unwrap();

            let versions: Vec<(crate::metadata::Metadata, Doc)> =
                col.history(&store, id).try_collect().await.unwrap();
            assert_eq!(
                versions.iter().map(|(_, d)| d.n).collect::<Vec<_>>(),
                vec![3, 2, 1],
                "history must walk newest-first"
            );

            // Chain shape: live → a2 → a1; forward links a1 → a2 → (live).
            let (live_meta, _) = &versions[0];
            let (v2_meta, _) = &versions[1];
            let (v1_meta, _) = &versions[2];
            let a2 = live_meta.old_modification_id.expect("live chains back");
            assert!(live_meta.new_modification_id.is_none(), "live = None");
            assert_eq!(
                live_meta.pivot_id,
                Some(pivot),
                "insert stamps the pivot back-link, saves carry it"
            );
            assert!(
                v2_meta.new_modification_id.is_none(),
                "newest archive's successor is the live record"
            );
            assert_eq!(
                v1_meta.new_modification_id,
                Some(a2),
                "older archive forward-links the archive that superseded it"
            );
            assert!(v1_meta.old_modification_id.is_none(), "first version");

            // The live read is unaffected; `get` still yields the value only.
            assert_eq!(col.get(&store, id).await.unwrap(), Some(Doc { n: 3 }));
        });
    }

    // Unique anchors chain the same way through save_unique/unique_history.
    #[test]
    fn unique_save_chains_and_history_walks() {
        use crate::record::unique_history;

        block_on(async {
            let store = MemStore::default();
            // Never saved: empty history, no error.
            let none: Vec<(crate::metadata::Metadata, Settings)> =
                unique_history(&store, tenant())
                    .try_collect()
                    .await
                    .unwrap();
            assert!(none.is_empty());

            for volume in [1u64, 2, 3] {
                save_unique(&store, tenant(), &Settings { volume })
                    .await
                    .unwrap();
            }
            assert_eq!(
                get_unique::<Settings, _>(&store, tenant()).await.unwrap(),
                Some(Settings { volume: 3 })
            );
            let versions: Vec<(crate::metadata::Metadata, Settings)> =
                unique_history(&store, tenant())
                    .try_collect()
                    .await
                    .unwrap();
            assert_eq!(
                versions.iter().map(|(_, s)| s.volume).collect::<Vec<_>>(),
                vec![3, 2, 1]
            );
            assert!(versions[0].1.volume == 3);
            assert!(versions.last().unwrap().0.old_modification_id.is_none());
        });
    }

    // The whole secondary-index lifecycle over the derived-equivalent shape:
    // insert indexes, save re-keys only changed fields, remove de-indexes,
    // duplicates coexist, root moves land in the rewritten pivot.
    #[test]
    fn secondary_index_lifecycle() {
        use crate::index::IndexKey;

        async fn by_label(
            col: Collection<Tagged>,
            store: &MemStore,
            label: &str,
        ) -> Vec<u64> {
            let mut ns: Vec<u64> = col
                .search_by(
                    store,
                    0,
                    crate::index::Bound::Exact(label.key_bytes()),
                )
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .into_iter()
                .map(|(_, t)| t.n)
                .collect();
            ns.sort_unstable();
            ns
        }

        block_on(async {
            let store = MemStore::default();
            let pivot = Collection::<Tagged>::create(&store, tenant())
                .await
                .unwrap();
            let col = Collection::<Tagged>::at(pivot, tenant()).with_caps(4, 4);

            let mut ids = Vec::new();
            for (label, n) in
                [("red", 1u64), ("blue", 2), ("red", 3), ("green", 4)]
            {
                let t = Tagged {
                    label: label.into(),
                    n,
                };
                ids.push(col.insert(&store, &t).await.unwrap());
            }
            assert_eq!(by_label(col, &store, "red").await, vec![1, 3]);
            assert_eq!(by_label(col, &store, "blue").await, vec![2]);

            // save with a changed field re-keys that index.
            col.save(
                &store,
                ids[0],
                &Tagged {
                    label: "blue".into(),
                    n: 1,
                },
            )
            .await
            .unwrap();
            assert_eq!(by_label(col, &store, "red").await, vec![3]);
            assert_eq!(by_label(col, &store, "blue").await, vec![1, 2]);

            // save with the field unchanged only rewrites the record.
            col.save(
                &store,
                ids[1],
                &Tagged {
                    label: "blue".into(),
                    n: 22,
                },
            )
            .await
            .unwrap();
            assert_eq!(by_label(col, &store, "blue").await, vec![1, 22]);

            // remove de-indexes the record from its secondary too.
            assert!(col.remove(&store, ids[2]).await.unwrap());
            assert_eq!(by_label(col, &store, "red").await, Vec::<u64>::new());

            // Undeclared index = typed error, not a panic.
            let err = col
                .search_by(&store, 1, crate::index::Bound::All)
                .try_collect::<Vec<_>>()
                .await
                .unwrap_err();
            assert!(matches!(err, Error::SecondaryIndexOutOfRange(1)));

            // Enough inserts to split the secondary tree's root; a fresh
            // handle (same PivotId) sees them through the rewritten pivot.
            for n in 0..40u64 {
                col.insert(
                    &store,
                    &Tagged {
                        label: "bulk".into(),
                        n,
                    },
                )
                .await
                .unwrap();
            }
            let fresh =
                Collection::<Tagged>::at(pivot, tenant()).with_caps(4, 4);
            assert_eq!(
                by_label(fresh, &store, "bulk").await,
                (0..40).collect::<Vec<u64>>()
            );
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
