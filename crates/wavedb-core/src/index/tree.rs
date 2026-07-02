//! [`BpTree`] — the `Store`-generic B+tree indexing a NonUnique collection,
//! one node per [`Store`] value.
//!
//! ## What the tree keys on
//!
//! The tree is an ordered set of record [`LocalId`]s. A NonUnique record's
//! `LocalId` is `CREATED_AT` (8 B, most significant) then `FLAG|SALT` (2 B), so
//! ordering the `LocalId`s **is** chronological order, and the trailing salt
//! makes every key unique — no duplicate-key ambiguity. A leaf entry is just
//! the record's `LocalId`; that same value is both the search key and the
//! record pointer.
//!
//! ## Where it runs
//!
//! All I/O is delegated to [`Store`] (`get` + `apply`), so the same tree serves
//! the node's page engine and the browser's IndexedDB — backends are
//! interchangeable under it. The write paths are in the sibling modules:
//! insert/split in [`tree_insert`](super::tree_insert), remove/merge in
//! [`tree_delete`](super::tree_delete).

use std::collections::VecDeque;

use futures::Stream;

use crate::error::{Error, Result};
use crate::id::Id;
use crate::local_id::LocalId;
use crate::store::{Store, Write};
use crate::u48::U48;

use super::Bound;
use super::node::{NodeBody, mint_node_id};

/// Max keys in a leaf before it splits. Sized so a node fits the storage
/// engine's 32 KiB page with room to spare.
pub const DEFAULT_LEAF_CAP: usize = 1819;
/// Max `(separator, child)` entries in an internal node before it splits.
pub const DEFAULT_INTERNAL_CAP: usize = 1630;

/// A B+tree over a [`Store`], scoped to one tenant. Holds only its root pointer
/// and its split/merge capacities; nodes live in the `Store` keyed by
/// `LocalId::to_id(tenant)`.
///
/// Use one capacity configuration for a tree's whole lifetime — capacities are
/// a handle-side policy, not recorded in the nodes.
#[derive(Debug, Clone, Copy)]
pub struct BpTree {
    pub(super) root: LocalId,
    pub(super) tenant: U48,
    pub(super) leaf_cap: usize,
    pub(super) internal_cap: usize,
}

impl BpTree {
    /// Open an existing tree at `root` for `tenant` (default capacities).
    #[must_use]
    pub const fn at(root: LocalId, tenant: U48) -> Self {
        Self {
            root,
            tenant,
            leaf_cap: DEFAULT_LEAF_CAP,
            internal_cap: DEFAULT_INTERNAL_CAP,
        }
    }

    /// Override the node capacities (small caps make deep trees cheap to build
    /// in tests; production uses the defaults).
    #[must_use]
    pub const fn with_caps(mut self, leaf: usize, internal: usize) -> Self {
        self.leaf_cap = leaf;
        self.internal_cap = internal;
        self
    }

    /// Plan a fresh, empty tree: the handle plus the [`Write`] that persists its
    /// empty root leaf. The caller commits the write (alone or inside a larger
    /// atomic batch) and persists the root in its `Pivot`.
    #[must_use]
    pub fn plan_create(tenant: U48) -> (Self, Write) {
        let root = mint_node_id();
        let tree = Self::at(root, tenant);
        let write = tree.put(root, &NodeBody::Leaf(Vec::new()));
        (tree, write)
    }

    /// Create a fresh, empty tree: writes an empty leaf root and returns the
    /// handle. The caller persists the returned root in its `Pivot`.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure.
    pub async fn create<S: Store>(store: &S, tenant: U48) -> Result<Self> {
        let (tree, write) = Self::plan_create(tenant);
        store.apply(&[write]).await?;
        Ok(tree)
    }

    /// The current root pointer (changes when the root splits or collapses).
    #[must_use]
    pub const fn root(&self) -> LocalId {
        self.root
    }

    /// Whether `record_id` is present in the tree.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure.
    pub async fn contains<S: Store>(
        &self,
        store: &S,
        record_id: Id,
    ) -> Result<bool> {
        let target = LocalId::from_id(record_id);
        let mut node = self.root;
        loop {
            match self.load(store, node).await? {
                NodeBody::Leaf(keys) => {
                    return Ok(keys.binary_search(&target).is_ok());
                }
                NodeBody::Internal { leftmost, entries } => {
                    node = child_for(leftmost, &entries, target);
                }
            }
        }
    }

    /// Stream the record `Id`s in key order whose `CREATED_AT` falls in `bound`.
    ///
    /// Two-phase resolution (index → `Id`s → caller fetch) lives above this: the
    /// stream yields the record `Id`s; resolving them to bytes is the caller's
    /// `Store::get`. The descent prunes subtrees whose separator range cannot
    /// intersect the bound (see [`Bound::created_at_range`]).
    pub fn search<'a, S: Store>(
        &self,
        store: &'a S,
        bound: Bound,
    ) -> impl Stream<Item = Result<Id>> + use<'a, S> {
        let mut nodes: VecDeque<LocalId> = VecDeque::new();
        nodes.push_back(self.root);
        let init = WalkState {
            nodes,
            ready: VecDeque::new(),
            prune: bound.created_at_range(),
            bound,
            tenant: self.tenant,
        };

        futures::stream::unfold(init, move |mut st| async move {
            loop {
                if let Some(id) = st.ready.pop_front() {
                    return Some((Ok(id), st));
                }
                let node = st.nodes.pop_front()?;
                let bytes = match store.get(node.to_id(st.tenant)).await {
                    Ok(Some(b)) => b,
                    Ok(None) => {
                        return Some((Err(Error::BpTreeNodeMissing(node)), st));
                    }
                    Err(e) => return Some((Err(e), st)),
                };
                match NodeBody::from_bytes(&bytes) {
                    Ok(NodeBody::Leaf(keys)) => {
                        for k in keys {
                            if bound_matches(&st.bound, k) {
                                st.ready.push_back(k.to_id(st.tenant));
                            }
                        }
                    }
                    Ok(NodeBody::Internal { leftmost, entries }) => {
                        expand_children(&mut st, leftmost, &entries);
                    }
                    Err(e) => return Some((Err(e), st)),
                }
            }
        })
    }

    /// Load and decode the node at `id`.
    pub(super) async fn load<S: Store>(
        &self,
        store: &S,
        node: LocalId,
    ) -> Result<NodeBody> {
        let bytes = store
            .get(node.to_id(self.tenant))
            .await?
            .ok_or(Error::BpTreeNodeMissing(node))?;
        NodeBody::from_bytes(&bytes)
    }

    /// A `Put` write for `node`'s serialised bytes under this tenant.
    pub(super) fn put(&self, node: LocalId, n: &NodeBody) -> Write {
        Write::Put(node.to_id(self.tenant), n.to_bytes())
    }

    /// A `Remove` write freeing `node` under this tenant.
    pub(super) fn remove_node(&self, node: LocalId) -> Write {
        Write::Remove(node.to_id(self.tenant))
    }
}

/// One internal node on a descent path, kept so an upward split/merge knows
/// where to insert or drop a separator.
pub(super) struct PathFrame {
    pub node_id: LocalId,
    pub leftmost: LocalId,
    pub entries: Vec<(LocalId, LocalId)>,
    /// Slot the descent took (`0` = leftmost, `i + 1` = `entries[i]`).
    pub child_idx: usize,
}

/// State threaded through the `search` walk.
struct WalkState {
    nodes: VecDeque<LocalId>,
    ready: VecDeque<Id>,
    bound: Bound,
    /// Inclusive `CREATED_AT` window for descent pruning (`None` = no pruning).
    prune: Option<(u64, u64)>,
    tenant: U48,
}

/// Queue an internal node's children in key order, skipping subtrees whose
/// key window cannot intersect the bound.
fn expand_children(
    st: &mut WalkState,
    leftmost: LocalId,
    entries: &[(LocalId, LocalId)],
) {
    // Child i covers LocalIds in [sep_{i-1}, sep_i), so its CREATED_AT values
    // lie within [sep_{i-1}.key(), sep_i.key()] inclusive.
    let child_window = |i: usize| -> (u64, u64) {
        let min = if i == 0 { 0 } else { entries[i - 1].0.key() };
        let max = entries.get(i).map_or(u64::MAX, |(sep, _)| sep.key());
        (min, max)
    };
    let keep = |i: usize| -> bool {
        st.prune.is_none_or(|(lo, hi)| {
            let (min, max) = child_window(i);
            max >= lo && min <= hi
        })
    };
    // Push in reverse so the leftmost kept child ends up at the queue's front.
    for (i, (_, child)) in entries.iter().enumerate().rev() {
        if keep(i + 1) {
            st.nodes.push_front(*child);
        }
    }
    if keep(0) {
        st.nodes.push_front(leftmost);
    }
}

/// Does a record `LocalId`'s `CREATED_AT` (its `key`) satisfy the bound?
/// Bounds carry 8-byte big-endian `CREATED_AT` keys.
fn bound_matches(bound: &Bound, key: LocalId) -> bool {
    match bound {
        Bound::All => true,
        _ => bound.contains(&key.key().to_be_bytes()),
    }
}

/// The child pointer a `target` routes to (without its index).
pub(super) fn child_for(
    leftmost: LocalId,
    entries: &[(LocalId, LocalId)],
    target: LocalId,
) -> LocalId {
    child_and_index(leftmost, entries, target).0
}

/// The child pointer and its slot index (`0` = leftmost, `i + 1` = `entries[i]`).
pub(super) fn child_and_index(
    leftmost: LocalId,
    entries: &[(LocalId, LocalId)],
    target: LocalId,
) -> (LocalId, usize) {
    // Last separator `<= target`; if none, the leftmost child.
    match entries.binary_search_by(|(sep, _)| sep.cmp(&target)) {
        Ok(i) => (entries[i].1, i + 1),
        Err(0) => (leftmost, 0),
        Err(i) => (entries[i - 1].1, i),
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use futures::executor::block_on;

    use super::super::Bound;
    use super::super::mem_store::MemStore;
    use super::BpTree;
    use crate::id::Id;
    use crate::u48::U48;

    const TENANT: u32 = 7;

    fn rec(created_at: u64) -> Id {
        Id::new(
            created_at,
            U48::from(TENANT),
            false,
            (created_at & 0x7FFF) as u16,
        )
    }

    async fn all_keys(tree: &BpTree, store: &MemStore) -> Vec<u64> {
        tree.search(store, Bound::All)
            .map(|r| r.unwrap().key())
            .collect()
            .await
    }

    #[test]
    fn insert_and_search_in_order() {
        block_on(async {
            let store = MemStore::default();
            let mut tree =
                BpTree::create(&store, U48::from(TENANT)).await.unwrap();
            for k in [50u64, 10, 30, 20, 40] {
                tree.insert(&store, rec(k)).await.unwrap();
            }
            assert_eq!(all_keys(&tree, &store).await, vec![10, 20, 30, 40, 50]);
        });
    }

    #[test]
    fn insert_is_idempotent() {
        block_on(async {
            let store = MemStore::default();
            let mut tree =
                BpTree::create(&store, U48::from(TENANT)).await.unwrap();
            tree.insert(&store, rec(5)).await.unwrap();
            tree.insert(&store, rec(5)).await.unwrap();
            assert_eq!(all_keys(&tree, &store).await, vec![5]);
            assert!(tree.contains(&store, rec(5)).await.unwrap());
            assert!(!tree.contains(&store, rec(6)).await.unwrap());
        });
    }

    #[test]
    fn grows_multiple_levels_and_stays_sorted() {
        block_on(async {
            let store = MemStore::default();
            let mut tree = BpTree::create(&store, U48::from(TENANT))
                .await
                .unwrap()
                .with_caps(16, 16);
            let root0 = tree.root();
            // Scrambled inserts force leaf and internal splits at these caps.
            let mut expected: Vec<u64> = Vec::new();
            for i in 0..2000u64 {
                let k = i.wrapping_mul(2_654_435_761) % 1_000_003;
                tree.insert(&store, rec(k)).await.unwrap();
                expected.push(k);
            }
            expected.sort_unstable();
            expected.dedup();

            let got = all_keys(&tree, &store).await;
            assert!(got.windows(2).all(|w| w[0] < w[1]), "not strictly sorted");
            assert_eq!(got, expected, "tree lost or duplicated keys");
            assert_ne!(tree.root(), root0, "root never moved — did not grow");

            super::super::mem_store::check_invariants(
                &tree,
                &store,
                U48::from(TENANT),
            )
            .await;
        });
    }

    #[test]
    fn range_search_filters_by_created_at() {
        block_on(async {
            let store = MemStore::default();
            let mut tree =
                BpTree::create(&store, U48::from(TENANT)).await.unwrap();
            for k in 0..100u64 {
                tree.insert(&store, rec(k)).await.unwrap();
            }
            let bound = Bound::Range {
                lo: 20u64.to_be_bytes().to_vec(),
                hi: 25u64.to_be_bytes().to_vec(),
            };
            let got: Vec<u64> = tree
                .search(&store, bound)
                .map(|r| r.unwrap().key())
                .collect()
                .await;
            assert_eq!(got, vec![20, 21, 22, 23, 24]); // half-open
        });
    }

    #[test]
    fn range_search_prunes_but_misses_nothing_on_deep_trees() {
        block_on(async {
            let store = MemStore::default();
            let mut tree = BpTree::create(&store, U48::from(TENANT))
                .await
                .unwrap()
                .with_caps(8, 8);
            for k in 0..500u64 {
                tree.insert(&store, rec(k)).await.unwrap();
            }
            // Several bounds, compared against the brute-force expectation.
            for (lo, hi) in [(0u64, 10u64), (100, 130), (490, 600), (250, 250)]
            {
                let bound = Bound::Range {
                    lo: lo.to_be_bytes().to_vec(),
                    hi: hi.to_be_bytes().to_vec(),
                };
                let got: Vec<u64> = tree
                    .search(&store, bound)
                    .map(|r| r.unwrap().key())
                    .collect()
                    .await;
                let expected: Vec<u64> = (lo..hi.min(500)).collect();
                assert_eq!(got, expected, "range [{lo}, {hi})");
            }
            // Exact hits exactly one key even across pruned descents.
            let got: Vec<u64> = tree
                .search(&store, Bound::Exact(123u64.to_be_bytes().to_vec()))
                .map(|r| r.unwrap().key())
                .collect()
                .await;
            assert_eq!(got, vec![123]);
        });
    }

    #[test]
    fn empty_tree_searches_nothing() {
        block_on(async {
            let store = MemStore::default();
            let tree = BpTree::create(&store, U48::from(TENANT)).await.unwrap();
            assert!(all_keys(&tree, &store).await.is_empty());
        });
    }
}
