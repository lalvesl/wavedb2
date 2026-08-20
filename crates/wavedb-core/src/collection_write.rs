//! [`Collection`]'s mutating half — `insert` / `save` / `remove` — each one
//! atomic [`Store::apply`] batch: the record write, every touched B+tree node
//! (primary, secondary, and the recency/dead logs, via the trees' `plan_*`
//! planners), and the rewritten `Pivot` when any root moved.
//!
//! Every instant minted here (an insert's id, a save's version instant, a
//! removal's log key) lands strictly above the collection's **instant
//! floor** (see [`crate::collection_recency`]), so the recency and dead
//! logs only ever grow at their tails.

use crate::collection::Collection;
use crate::error::Result;
use crate::id::Id;
use crate::index::{BpTree, Chain, ChainRoots, LogRoots, Pivot, Roots, SecKey};
use crate::local_id::LocalId;
use crate::metadata::{Metadata, Succession};
use crate::notify::{Mutation, MutationKind};
use crate::record::{
    Overlay, encode_record, mint_floored_id, plan_chained_save,
};
use crate::store::{Store, Write};
use crate::traits::NonUniqueStruct;
use crate::wire::to_wire;

/// Everything one mutation may have moved, gathered so the `Pivot` is compared
/// and rewritten **exactly once** — a second write built from the pre-batch
/// pivot would drop whatever the first one carried.
pub struct MovedRoots<'a> {
    /// The secondary trees, in declaration order.
    pub secondaries: &'a [BpTree<SecKey>],
    /// The record chain's endpoints and index root (RFC 0050).
    pub records: ChainRoots,
    /// The removal log's endpoints.
    pub removals: LogRoots,
    /// The declared lists' chains, in declaration order (RFC 0051).
    pub lists: &'a [Chain<Vec<u8>>],
}

impl<T: NonUniqueStruct> Collection<T> {
    /// When any root moved — a secondary B+tree's, or a chain's endpoint —
    /// append the `Pivot` rewrite carrying them all.
    ///
    /// A chain moves an endpoint at most once in its life (its first split) and
    /// its index root never moves, so a collection with no declared secondary
    /// index compares and writes nothing here after that one split. What used
    /// to make this fire constantly — `current`, `recency`, `dead` — is gone
    /// (RFC 0050 phase 5c).
    pub(crate) fn push_root_moves(
        &self,
        batch: &mut Vec<Write>,
        pivot: &T::Pivot,
        moved: &MovedRoots<'_>,
    ) {
        let sec_roots: Vec<LocalId> =
            moved.secondaries.iter().map(BpTree::root).collect();
        let list_roots: Vec<ChainRoots> =
            moved.lists.iter().map(Chain::roots).collect();
        if sec_roots.as_slice() != pivot.secondaries()
            || moved.records != pivot.records()
            || moved.removals != pivot.removals()
            || list_roots.as_slice() != pivot.lists()
        {
            let rewritten = pivot.replace_roots(Roots {
                secondaries: &sec_roots,
                records: moved.records,
                removals: moved.removals,
                lists: &list_roots,
            });
            batch.push(self.pivot_rewrite(&rewritten));
        }
    }

    /// Insert `value` as a new record: mints its timestamp-keyed [`Id`] (the
    /// stable identity for the record's whole life), writes the record,
    /// indexes it in `current`, the recency log, and every secondary tree,
    /// and rewrites the `Pivot` if any root moved — one atomic batch.
    /// Returns the minted `Id`.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure, [`Error::PivotMissing`](crate::Error::PivotMissing)
    /// on a stale handle, or a decode fault on a corrupt pivot.
    pub async fn insert<S: Store>(&self, store: &S, value: &T) -> Result<Id> {
        let pivot = self.load_pivot(store).await?;
        // A `#[wavedb::key(...)]` type's identity is its key fields —
        // insert IS the upsert at the content-derived anchor (see
        // `collection_keyed`).
        if let Some(id) = self.keyed_anchor(value) {
            self.upsert_keyed(store, &pivot, id, value).await?;
            return Ok(id);
        }
        let floor = self.instant_floor(store, &pivot).await?;
        let id = mint_floored_id(self.tenant(), T::STRUCT_HASH, floor);
        self.insert_at(store, &pivot, id, value).await?;
        Ok(id)
    }

    /// [`insert`](Self::insert)'s body with the identity supplied instead of
    /// minted — the seam [`adopt`](Self::adopt) writes a **node-minted** `Id`
    /// through when mirroring a record into a local cache store.
    pub(crate) async fn insert_at<S: Store>(
        &self,
        store: &S,
        pivot: &T::Pivot,
        id: Id,
        value: &T,
    ) -> Result<()> {
        // First version: no chain yet; the pivot back-link is stamped here
        // (the future handle-less `record.save(&db)` reaches roots through it)
        // and the writer identity is the tenant until node auth exists (M8).
        // The authoring instant is the id's own key — the key IS when that
        // version was authored.
        let meta = Metadata {
            succession: Succession::CreatedAt(id.key()),
            pivot_id: Some(self.pivot()),
            user: self.user(),
            ..Metadata::default()
        };
        self.insert_with_meta(store, pivot, id, meta, value).await
    }

    /// The indexed first-version write, with the [`Metadata`] supplied —
    /// [`adopt_with`](Self::adopt_with) passes the **node's** verbatim, so a
    /// mirror carries authoritative chain data.
    pub(crate) async fn insert_with_meta<S: Store>(
        &self,
        store: &S,
        pivot: &T::Pivot,
        id: Id,
        meta: Metadata,
        value: &T,
    ) -> Result<()> {
        // A first version is live by definition; its instant keys the
        // recency entry.
        let Succession::CreatedAt(instant) = meta.succession else {
            return Err(crate::Error::ChainCorrupt(id));
        };
        let mut records = self.records_chain(pivot);
        let mut secs = self.sec_trees(pivot);
        let mut lists = self.list_chains(pivot);
        let envelope = encode_record(T::STRUCT_HASH, &meta, value);
        let key = Self::instant_key(instant, LocalId::from_id(id));
        // The chain carries the record **inline**, keyed by the instant its
        // live version was authored. That one entry is the membership set and
        // the modification log at once (RFC 0050).
        let mut batch = vec![Write::Put(id, envelope.clone())];
        batch.extend(records.plan_insert(store, key, envelope.clone()).await?);
        // …and one more copy per declared list, sorted by its own property
        // (RFC 0051).
        self.plan_list_inserts(
            store, &mut batch, &mut lists, id, &envelope, value,
        )
        .await?;
        for (i, tree) in secs.iter_mut().enumerate() {
            let key = Self::sec_key(value, i, id);
            batch.extend(tree.plan_insert(store, key).await?);
        }
        self.push_root_moves(
            &mut batch,
            pivot,
            &MovedRoots {
                secondaries: &secs,
                records: records.roots(),
                removals: pivot.removals(),
                lists: &lists,
            },
        );
        store.apply(&batch).await?;
        store.note_mutation(|| Mutation {
            struct_hash: T::STRUCT_HASH,
            tenant: self.tenant(),
            pivot: Some(self.pivot()),
            id,
            kind: MutationKind::Saved,
            meta: Some(meta),
            body: to_wire(value),
        });
        Ok(())
    }

    /// Overwrite the record at `id` with `value` — the NonUnique *update*.
    /// The superseded version is **archived** (its bytes move to a fresh id)
    /// and the modification chain linked, so the timeline stays walkable via
    /// [`history`](Collection::history). The `Id` (and so the record's place
    /// in `current`) is unchanged — the primary tree never reindexes; each
    /// **secondary** tree re-keys when its field values changed (old key out,
    /// new key in), and the recency entry re-keys to the new version's
    /// instant. Everything commits as one atomic batch.
    ///
    /// A save to a never-inserted `id` writes an unindexed, chainless first
    /// version (Unique-style upsert semantics) — but a secondary-indexed
    /// type's fields would then be invisible to `by_` lookups, so its save
    /// requires an existing record.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure or a decode fault,
    /// [`Error::PivotMissing`] on a stale handle, or
    /// [`Error::RecordMissing`] when a secondary-indexed `id` was never
    /// inserted.
    ///
    /// [`Error::PivotMissing`]: crate::Error::PivotMissing
    /// [`Error::RecordMissing`]: crate::Error::RecordMissing
    pub async fn save<S: Store>(
        &self,
        store: &S,
        id: Id,
        value: &T,
    ) -> Result<()> {
        self.save_planned(store, id, value, None).await
    }

    /// [`save`](Self::save) with the node's [`Metadata`] written verbatim —
    /// [`adopt_with`](Self::adopt_with)'s update half.
    pub(crate) async fn save_with_meta<S: Store>(
        &self,
        store: &S,
        id: Id,
        meta: Metadata,
        value: &T,
    ) -> Result<()> {
        self.save_planned(store, id, value, Some(meta)).await
    }

    pub(crate) async fn save_planned<S: Store>(
        &self,
        store: &S,
        id: Id,
        value: &T,
        imposed: Option<Metadata>,
    ) -> Result<()> {
        // A keyed value may never be saved under a foreign anchor — the
        // key fields ARE the identity ("renaming" = remove + insert).
        if let Some(anchor) = self.keyed_anchor(value)
            && anchor != id
        {
            return Err(crate::Error::KeyMismatch(id));
        }
        let pivot = self.load_pivot(store).await?;
        let floor = self.instant_floor(store, &pivot).await?;
        let plan = crate::record::SavePlan {
            hash: T::STRUCT_HASH,
            shape: T::SHAPE,
            live_id: id,
            tenant: self.tenant(),
            user: self.user(),
            pivot_id: Some(self.pivot()),
            imposed,
            floor,
            revives: false,
        };
        let (mut batch, old, live_meta) =
            plan_chained_save::<T, S>(store, &plan, value).await?;
        let mut records = self.records_chain(&pivot);
        let mut secs = self.sec_trees(&pivot);
        let mut lists = self.list_chains(&pivot);
        // Removals and inserts mutate the same structures in one batch: each
        // plan reads through the overlay of the pending node writes.
        let mut view = Overlay::new(store);
        if let Some((old_instant, old_value)) = &old {
            let anchor = LocalId::from_id(id);
            // A save **relocates** the record in the chain: its key is the live
            // version's authoring instant, and that instant just changed. Out of
            // the old position, in at the new one — which is the movement that
            // *is* the modification log (RFC 0050).
            let moved = self
                .plan_chain_move(
                    &mut view,
                    &mut batch,
                    &mut records,
                    Self::instant_key(*old_instant, anchor),
                    &live_meta,
                    value,
                )
                .await?;
            // Only a record the built-in chain holds belongs in a declared list
            // — the same liveness gate, applied once (RFC 0051).
            if let Some(envelope) = moved {
                self.plan_list_moves(
                    &mut view,
                    &mut batch,
                    &mut lists,
                    id,
                    &envelope,
                    (old_value, value),
                )
                .await?;
            }
            for (i, tree) in secs.iter_mut().enumerate() {
                let old_key = Self::sec_key(old_value, i, id);
                let new_key = Self::sec_key(value, i, id);
                if old_key == new_key {
                    continue; // this index's fields didn't change
                }
                if let Some(writes) = tree.plan_remove(&view, old_key).await? {
                    view.stage(&writes);
                    batch.extend(writes);
                }
                let writes = tree.plan_insert(&view, new_key).await?;
                view.stage(&writes);
                batch.extend(writes);
            }
        } else if T::NUM_SECONDARIES > 0 {
            return Err(crate::Error::RecordMissing(id));
        }
        self.push_root_moves(
            &mut batch,
            &pivot,
            &MovedRoots {
                secondaries: &secs,
                records: records.roots(),
                removals: pivot.removals(),
                lists: &lists,
            },
        );
        store.apply(&batch).await?;
        store.note_mutation(|| Mutation {
            struct_hash: T::STRUCT_HASH,
            tenant: self.tenant(),
            pivot: Some(self.pivot()),
            id,
            kind: MutationKind::Saved,
            meta: Some(live_meta),
            body: to_wire(value),
        });
        Ok(())
    }
}
