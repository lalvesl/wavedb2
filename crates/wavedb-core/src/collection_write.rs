//! [`Collection`]'s mutating half — `insert` / `save` / `remove` — each one
//! atomic [`Store::apply`] batch: the record write, every touched B+tree node
//! (primary **and** secondary, via the trees' `plan_*` planners), and the
//! rewritten `Pivot` when any root moved.

use crate::collection::Collection;
use crate::error::Result;
use crate::id::Id;
use crate::index::{BpTree, Pivot, SecKey};
use crate::local_id::LocalId;
use crate::metadata::Metadata;
use crate::record::{
    Overlay, encode_record, mint_timestamped_id, plan_chained_save,
};
use crate::store::{Store, Write};
use crate::traits::NonUniqueStruct;

impl<T: NonUniqueStruct> Collection<T> {
    /// When any tree root moved, append the `Pivot` rewrite carrying them all.
    fn push_root_moves(
        &self,
        batch: &mut Vec<Write>,
        pivot: &T::Pivot,
        current: LocalId,
        dead: LocalId,
        secs: &[BpTree<SecKey>],
    ) {
        let sec_roots: Vec<LocalId> = secs.iter().map(BpTree::root).collect();
        if current != pivot.current()
            || dead != pivot.dead()
            || sec_roots.as_slice() != pivot.secondaries()
        {
            let moved = pivot.replace_roots(current, dead, &sec_roots);
            batch.push(self.pivot_rewrite(&moved));
        }
    }

    /// Insert `value` as a new record: mints its timestamp-keyed [`Id`] (the
    /// stable identity for the record's whole life), writes the record,
    /// indexes it in `current` and in every secondary tree, and rewrites the
    /// `Pivot` if any root moved — one atomic batch. Returns the minted `Id`.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure, [`Error::PivotMissing`] on a stale
    /// handle, or a decode fault on a corrupt pivot.
    pub async fn insert<S: Store>(&self, store: &S, value: &T) -> Result<Id> {
        let pivot = self.load_pivot(store).await?;
        let mut current = self.tree(pivot.current());
        let mut secs = self.sec_trees(&pivot);

        let id = mint_timestamped_id(self.tenant());
        // First version: no chain yet; the pivot back-link is stamped here
        // (the future handle-less `record.save(&db)` reaches roots through it)
        // and the writer identity is the tenant until node auth exists (M8).
        let meta = Metadata {
            pivot_id: Some(self.pivot()),
            user: self.user(),
            ..Metadata::default()
        };
        let mut batch =
            vec![Write::Put(id, encode_record(T::STRUCT_HASH, &meta, value))];
        batch.extend(current.plan_insert(store, id).await?);
        for (i, tree) in secs.iter_mut().enumerate() {
            let key = Self::sec_key(value, i, id);
            batch.extend(tree.plan_insert(store, key).await?);
        }
        self.push_root_moves(
            &mut batch,
            &pivot,
            current.root(),
            pivot.dead(),
            &secs,
        );
        store.apply(&batch).await?;
        Ok(id)
    }

    /// Remove the record at `id` from the living set: de-indexes it from
    /// `current` **and every secondary tree**, and indexes it in `dead` — one
    /// atomic batch. The record **bytes stay** (history stays navigable); only
    /// `remove` ever writes the `dead` tree. Returns whether the record was in
    /// `current`.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure, [`Error::PivotMissing`] on a stale
    /// handle, [`Error::RecordMissing`] if a secondary-indexed record's bytes
    /// are gone (index out of sync), or a decode fault on a corrupt pivot.
    pub async fn remove<S: Store>(&self, store: &S, id: Id) -> Result<bool> {
        let pivot = self.load_pivot(store).await?;
        let mut current = self.tree(pivot.current());
        let mut dead = self.tree(pivot.dead());
        let mut secs = self.sec_trees(&pivot);

        let Some(mut batch) = current.plan_remove(store, id).await? else {
            return Ok(false);
        };
        batch.extend(dead.plan_insert(store, id).await?);
        if T::NUM_SECONDARIES > 0 {
            // The record's bytes carry the field values its secondary keys
            // were built from.
            let (_, value) = self.load_record(store, id).await?;
            for (i, tree) in secs.iter_mut().enumerate() {
                let key = Self::sec_key(&value, i, id);
                if let Some(writes) = tree.plan_remove(store, key).await? {
                    batch.extend(writes);
                }
            }
        }
        self.push_root_moves(
            &mut batch,
            &pivot,
            current.root(),
            dead.root(),
            &secs,
        );
        store.apply(&batch).await?;
        Ok(true)
    }

    /// Overwrite the record at `id` with `value` — the NonUnique *update*.
    /// The superseded version is **archived** (its bytes move to a fresh id)
    /// and the modification chain linked, so the timeline stays walkable via
    /// [`history`](Collection::history). The `Id` (and so the record's place
    /// in `current`) is unchanged — the primary tree never reindexes; each
    /// **secondary** tree re-keys when its field values changed (old key out,
    /// new key in). Everything commits as one atomic batch.
    ///
    /// A save to a never-inserted `id` writes an unindexed, chainless first
    /// version (Unique-style upsert semantics) — but a secondary-indexed
    /// type's fields would then be invisible to `by_` lookups, so its save
    /// requires an existing record.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure or a decode fault; with secondaries
    /// also [`Error::PivotMissing`] on a stale handle or
    /// [`Error::RecordMissing`] when `id` was never inserted.
    ///
    /// [`Error::PivotMissing`]: crate::Error::PivotMissing
    /// [`Error::RecordMissing`]: crate::Error::RecordMissing
    pub async fn save<S: Store>(
        &self,
        store: &S,
        id: Id,
        value: &T,
    ) -> Result<()> {
        let (mut batch, old) = plan_chained_save::<T, S>(
            store,
            T::STRUCT_HASH,
            id,
            self.tenant(),
            self.user(),
            value,
            Some(self.pivot()),
        )
        .await?;
        if T::NUM_SECONDARIES > 0 {
            let Some((_, old_value)) = old else {
                return Err(crate::Error::RecordMissing(id));
            };
            let pivot = self.load_pivot(store).await?;
            let mut secs = self.sec_trees(&pivot);
            // The removal and the insert mutate the same tree in one batch:
            // the insert must plan against the removal's pending node writes.
            let mut view = Overlay::new(store);
            for (i, tree) in secs.iter_mut().enumerate() {
                let old_key = Self::sec_key(&old_value, i, id);
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
            self.push_root_moves(
                &mut batch,
                &pivot,
                pivot.current(),
                pivot.dead(),
                &secs,
            );
        }
        store.apply(&batch).await
    }
}
