//! [`BpTree::insert`] — descend, insert into the leaf, split upward on
//! overflow, grow a new root above the old one when the split reaches it.

use crate::error::Result;
use crate::id::Id;
use crate::local_id::LocalId;
use crate::store::{Store, Write};

use super::node::{NodeBody, mint_node_id};
use super::tree::{BpTree, PathFrame, child_and_index};

impl BpTree {
    /// Insert `record_id`. Idempotent: inserting a key already present is a
    /// no-op. Updates [`root`](Self::root) if the tree grew a level. All
    /// touched nodes commit in **one** [`Store::apply`] batch.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure.
    pub async fn insert<S: Store>(
        &mut self,
        store: &S,
        record_id: Id,
    ) -> Result<()> {
        let batch = self.plan_insert(store, record_id).await?;
        store.apply(&batch).await
    }

    /// Plan an insert of `record_id`: every node [`Write`] the insert needs,
    /// **without applying** — so a caller can commit the index change and its
    /// record in one atomic batch. Reads through `store`; updates
    /// [`root`](Self::root) if the tree grew a level (the handle assumes the
    /// batch will be applied). An empty batch = key already present.
    ///
    /// # Errors
    /// Propagates a [`Store`] read failure.
    pub async fn plan_insert<S: Store>(
        &mut self,
        store: &S,
        record_id: Id,
    ) -> Result<Vec<Write>> {
        let target = LocalId::from_id(record_id);

        // Descend to the leaf, recording the internal path for split propagation.
        let mut path: Vec<PathFrame> = Vec::new();
        let mut node_id = self.root;
        let leaf_keys = loop {
            match self.load(store, node_id).await? {
                NodeBody::Leaf(keys) => break keys,
                NodeBody::Internal { leftmost, entries } => {
                    let (next, child_idx) =
                        child_and_index(leftmost, &entries, target);
                    path.push(PathFrame {
                        node_id,
                        leftmost,
                        entries,
                        child_idx,
                    });
                    node_id = next;
                }
            }
        };

        let mut keys = leaf_keys;
        match keys.binary_search(&target) {
            Ok(_) => return Ok(Vec::new()), // already present
            Err(pos) => keys.insert(pos, target),
        }

        let mut batch: Vec<Write> = Vec::new();

        // No split: rewrite the leaf and we're done.
        if keys.len() <= self.leaf_cap {
            batch.push(self.put(node_id, &NodeBody::Leaf(keys)));
            return Ok(batch);
        }

        // Split the leaf: keep the left half at `node_id`, mint the right half.
        let mid = keys.len() / 2;
        let right_keys = keys.split_off(mid);
        let sep = right_keys[0];
        let right_id = mint_node_id();
        batch.push(self.put(node_id, &NodeBody::Leaf(keys)));
        batch.push(self.put(right_id, &NodeBody::Leaf(right_keys)));

        // Propagate (sep, right_id) up the recorded path, splitting as needed.
        let mut pending = Some((sep, right_id));
        while let Some((sep, right)) = pending.take() {
            let Some(PathFrame {
                node_id: parent_id,
                leftmost,
                mut entries,
                child_idx,
            }) = path.pop()
            else {
                // Above the old root: grow a new internal root. The node that
                // just split was the root, and its left half kept the root's
                // `LocalId` (`node_id`), so that becomes the new leftmost child.
                let new_root = mint_node_id();
                batch.push(self.put(
                    new_root,
                    &NodeBody::Internal {
                        leftmost: node_id,
                        entries: vec![(sep, right)],
                    },
                ));
                self.root = new_root;
                return Ok(batch);
            };

            // Insert the new separator just after the child we descended into.
            entries.insert(child_idx, (sep, right));
            if entries.len() <= self.internal_cap {
                batch.push(
                    self.put(
                        parent_id,
                        &NodeBody::Internal { leftmost, entries },
                    ),
                );
                return Ok(batch);
            }

            // Split the internal node; promote the median separator.
            let mid = entries.len() / 2;
            let promote = entries[mid];
            let left_entries = entries[..mid].to_vec();
            let right_entries = entries[mid + 1..].to_vec();
            let right_internal = mint_node_id();
            batch.push(self.put(
                parent_id,
                &NodeBody::Internal {
                    leftmost,
                    entries: left_entries,
                },
            ));
            batch.push(self.put(
                right_internal,
                &NodeBody::Internal {
                    leftmost: promote.1,
                    entries: right_entries,
                },
            ));
            // `parent_id` takes the role of the descended child for the next level.
            node_id = parent_id;
            pending = Some((promote.0, right_internal));
        }

        Ok(batch)
    }
}
