//! [`PageBpTree`] — the `Store`-generic B+tree that indexes a NonUnique
//! collection, one node per [`Store`] value.
//!
//! ## What the tree keys on
//!
//! The tree is the **primary `current` index**: an ordered set of record
//! [`LocalId`]s. A NonUnique record's `LocalId` is `CREATED_AT` (8 B, most
//! significant) then `FLAG|SALT` (2 B), so ordering the `LocalId`s **is**
//! chronological order, and the trailing salt makes every key unique — no
//! duplicate-key ambiguity. So a leaf entry is just the record's `LocalId`, and
//! that same value is both the search key and the record pointer.
//!
//! ## Node layout (one node per Store value)
//!
//! ```text
//! leaf      = [ kind=0 (u8) ][ count (u16) ][ LocalId (10) × count ]
//! internal  = [ kind=1 (u8) ][ count (u16) ][ leftmost child (10) ]
//!                                           [ (sep LocalId (10), child (10)) × count ]
//! ```
//!
//! An internal node with `count` separators has `count + 1` children: a `leftmost`
//! covering keys below `sep[0]`, then one child per separator covering keys `≥`
//! that separator. Both node kinds cap well under a 32 KiB page.
//!
//! ## Node identity
//!
//! Node Store-keys are minted with `FLAG = 1`, which namespaces them away from the
//! `FLAG = 0` record keys that share the tenant's keyspace — a node key and a
//! record key can never collide. Minting is non-deterministic (clock + counter),
//! which is fine: nodes are persisted by value in the journal, so replay
//! reproduces the exact bytes without re-minting.
//!
//! ## Scope (M2)
//!
//! `insert` does full leaf/internal split with cascade and root growth. `remove`
//! deletes the key from its leaf; node **merge/rebalance on underflow is a
//! documented follow-up** (lookups stay correct meanwhile — an underfull node is
//! still a valid node, only space is left unreclaimed). `search` streams record
//! `Id`s in key order, filtered by a `CREATED_AT` [`Bound`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::Stream;
use wavedb_core::{
    Bound, Id, LocalId, Result, Store, U48, Write, from_wire, to_wire,
};

use crate::error::{StorageError, StorageResult};

const KIND_LEAF: u8 = 0;
const KIND_INTERNAL: u8 = 1;
const LOCALID_LEN: usize = 10;

/// Reserved `STRUCT_HASH` stamped at the head of every BpTree node's bytes.
///
/// The storage layer keys pages/records by the `STRUCT_HASH` in their first 8
/// bytes (see [`crate::page_store`]); BpTree nodes are a page kind too, so they
/// carry this constant there. It makes a node an ordinary record value to any
/// `Store` — no node-vs-record special-casing — and routes all nodes into one
/// reserved directory. (A real struct hashing to this exact value is a 2⁻⁶⁴
/// event and merely shares that directory, harmlessly.)
const BPTREE_NODE_STRUCT_HASH: u64 = 0x42_50_54_52_45_45_00_01; // "BPTREE\0\x01"
/// Bytes before a node's own header: the reserved `STRUCT_HASH`.
const NODE_PREFIX: usize = 8;

/// Max separators in a leaf / internal node before it splits. Both fit a 32 KiB
/// page with room to spare (`3 + 1819·10` and `3 + 10 + 1630·20`).
const LEAF_CAP: usize = 1819;
const INTERNAL_CAP: usize = 1630;

/// Process-wide salt counter so two nodes minted in the same nanosecond still get
/// distinct `LocalId`s.
static NODE_SALT: AtomicU64 = AtomicU64::new(0);

/// A B+tree over a [`Store`], scoped to one tenant. Holds only its root pointer;
/// nodes live in the `Store` keyed by `LocalId.to_id(tenant)`.
#[derive(Debug, Clone, Copy)]
pub struct PageBpTree {
    root: LocalId,
    tenant: U48,
}

/// A node, decoded from its `Store` bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Leaf(Vec<LocalId>),
    Internal {
        leftmost: LocalId,
        /// `(separator, child)` pairs, ascending by separator.
        entries: Vec<(LocalId, LocalId)>,
    },
}

impl PageBpTree {
    /// Open an existing tree at `root` for `tenant`.
    #[must_use]
    pub const fn at(root: LocalId, tenant: U48) -> Self {
        Self { root, tenant }
    }

    /// Create a fresh, empty tree: writes an empty leaf root and returns the
    /// handle. The caller persists the returned root in its `Pivot`.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure.
    pub async fn create<S: Store>(store: &S, tenant: U48) -> Result<Self> {
        let root = mint_node_id();
        store
            .apply(&[Write::Put(
                root.to_id(tenant),
                Node::Leaf(Vec::new()).to_bytes(),
            )])
            .await?;
        Ok(Self { root, tenant })
    }

    /// The current root pointer (changes only when the root splits).
    #[must_use]
    pub const fn root(&self) -> LocalId {
        self.root
    }

    async fn load<S: Store>(&self, store: &S, node: LocalId) -> Result<Node> {
        let bytes = store
            .get(node.to_id(self.tenant))
            .await?
            .ok_or(StorageError::BpTreeNodeMissing(node))?;
        Ok(Node::from_bytes(&bytes)?)
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
                Node::Leaf(keys) => {
                    return Ok(keys.binary_search(&target).is_ok());
                }
                Node::Internal { leftmost, entries } => {
                    node = child_for(leftmost, &entries, target);
                }
            }
        }
    }

    /// Insert `record_id`. Idempotent: inserting a key already present is a no-op.
    /// Updates [`root`](Self::root) if the tree grew a level.
    ///
    /// # Errors
    /// Propagates a [`Store`] failure.
    pub async fn insert<S: Store>(
        &mut self,
        store: &S,
        record_id: Id,
    ) -> Result<()> {
        let target = LocalId::from_id(record_id);

        // Descend to the leaf, recording the internal path for split propagation.
        let mut path: Vec<PathFrame> = Vec::new();
        let mut node_id = self.root;
        let leaf_keys = loop {
            match self.load(store, node_id).await? {
                Node::Leaf(keys) => break keys,
                Node::Internal { leftmost, entries } => {
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
            Ok(_) => return Ok(()), // already present
            Err(pos) => keys.insert(pos, target),
        }

        let mut batch: Vec<Write> = Vec::new();

        // No split: rewrite the leaf and we're done.
        if keys.len() <= LEAF_CAP {
            batch.push(self.put(node_id, &Node::Leaf(keys)));
            return store.apply(&batch).await;
        }

        // Split the leaf: keep the left half at `node_id`, mint the right half.
        let mid = keys.len() / 2;
        let right_keys = keys.split_off(mid);
        let sep = right_keys[0];
        let right_id = mint_node_id();
        batch.push(self.put(node_id, &Node::Leaf(keys)));
        batch.push(self.put(right_id, &Node::Leaf(right_keys)));

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
                    &Node::Internal {
                        leftmost: node_id,
                        entries: vec![(sep, right)],
                    },
                ));
                self.root = new_root;
                return store.apply(&batch).await;
            };

            // Insert the new separator just after the child we descended into.
            entries.insert(child_idx, (sep, right));
            if entries.len() <= INTERNAL_CAP {
                batch.push(
                    self.put(parent_id, &Node::Internal { leftmost, entries }),
                );
                return store.apply(&batch).await;
            }

            // Split the internal node; promote the median separator.
            let mid = entries.len() / 2;
            let promote = entries[mid];
            let left_entries = entries[..mid].to_vec();
            let right_entries = entries[mid + 1..].to_vec();
            let right_internal = mint_node_id();
            batch.push(self.put(
                parent_id,
                &Node::Internal {
                    leftmost,
                    entries: left_entries,
                },
            ));
            batch.push(self.put(
                right_internal,
                &Node::Internal {
                    leftmost: promote.1,
                    entries: right_entries,
                },
            ));
            // `parent_id` takes the role of the descended child for the next level.
            node_id = parent_id;
            pending = Some((promote.0, right_internal));
        }

        store.apply(&batch).await
    }

    /// Remove `record_id` from its leaf. Returns whether it was present.
    /// (No node merge yet — see the module note.)
    ///
    /// # Errors
    /// Propagates a [`Store`] failure.
    pub async fn remove<S: Store>(
        &mut self,
        store: &S,
        record_id: Id,
    ) -> Result<bool> {
        let target = LocalId::from_id(record_id);
        let mut node_id = self.root;
        loop {
            match self.load(store, node_id).await? {
                Node::Leaf(mut keys) => match keys.binary_search(&target) {
                    Ok(pos) => {
                        keys.remove(pos);
                        store
                            .apply(&[self.put(node_id, &Node::Leaf(keys))])
                            .await?;
                        return Ok(true);
                    }
                    Err(_) => return Ok(false),
                },
                Node::Internal { leftmost, entries } => {
                    node_id = child_for(leftmost, &entries, target);
                }
            }
        }
    }

    /// Stream the record `Id`s in key order whose `CREATED_AT` falls in `bound`.
    ///
    /// Two-phase resolution (index → `Id`s → caller fetch) lives above this: the
    /// stream yields the record `Id`s; resolving them to bytes is the caller's
    /// `Store::get`.
    pub fn search<'a, S: Store>(
        &self,
        store: &'a S,
        bound: Bound,
    ) -> impl Stream<Item = Result<Id>> + 'a {
        let tenant = self.tenant;
        // Walk state: nodes still to expand (in key order) + decoded leaf keys ready.
        let mut nodes: VecDeque<LocalId> = VecDeque::new();
        nodes.push_back(self.root);
        let init = WalkState {
            nodes,
            ready: VecDeque::new(),
            bound,
            tenant,
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
                        return Some((
                            Err(StorageError::BpTreeNodeMissing(node).into()),
                            st,
                        ));
                    }
                    Err(e) => return Some((Err(e), st)),
                };
                match Node::from_bytes(&bytes) {
                    Ok(Node::Leaf(keys)) => {
                        for k in keys {
                            if bound_contains_created_at(&st.bound, k) {
                                st.ready.push_back(k.to_id(st.tenant));
                            }
                        }
                    }
                    Ok(Node::Internal { leftmost, entries }) => {
                        // Push children in reverse so the leftmost ends up at front.
                        for (_, child) in entries.iter().rev() {
                            st.nodes.push_front(*child);
                        }
                        st.nodes.push_front(leftmost);
                    }
                    Err(e) => return Some((Err(e.into()), st)),
                }
            }
        })
    }

    fn put(&self, node: LocalId, n: &Node) -> Write {
        Write::Put(node.to_id(self.tenant), n.to_bytes())
    }
}

/// One internal node on the descent path, kept so an upward split knows where to
/// insert the promoted separator.
struct PathFrame {
    node_id: LocalId,
    leftmost: LocalId,
    entries: Vec<(LocalId, LocalId)>,
    /// Slot the descent took (`0` = leftmost, `i+1` = `entries[i]`).
    child_idx: usize,
}

/// State threaded through the `search` walk.
struct WalkState {
    nodes: VecDeque<LocalId>,
    ready: VecDeque<Id>,
    bound: Bound,
    tenant: U48,
}

/// The child pointer a `target` routes to (without its index).
fn child_for(
    leftmost: LocalId,
    entries: &[(LocalId, LocalId)],
    target: LocalId,
) -> LocalId {
    child_and_index(leftmost, entries, target).0
}

/// The child pointer and its slot index (`0` = leftmost, `i+1` = `entries[i]`).
fn child_and_index(
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

/// Does a record `LocalId`'s `CREATED_AT` (its `key`) satisfy a `CREATED_AT`
/// bound? Bounds carry 8-byte big-endian `CREATED_AT` keys.
fn bound_contains_created_at(bound: &Bound, key: LocalId) -> bool {
    match bound {
        Bound::All => true,
        _ => bound.contains(&key.key().to_be_bytes()),
    }
}

impl Node {
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&BPTREE_NODE_STRUCT_HASH.to_le_bytes()); // page-kind tag
        match self {
            Self::Leaf(keys) => {
                out.push(KIND_LEAF);
                out.extend_from_slice(&(keys.len() as u16).to_le_bytes());
                for k in keys {
                    out.extend_from_slice(&to_wire(k));
                }
            }
            Self::Internal { leftmost, entries } => {
                out.push(KIND_INTERNAL);
                out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
                out.extend_from_slice(&to_wire(leftmost));
                for (sep, child) in entries {
                    out.extend_from_slice(&to_wire(sep));
                    out.extend_from_slice(&to_wire(child));
                }
            }
        }
        out
    }

    fn from_bytes(buf: &[u8]) -> StorageResult<Self> {
        if buf.len() < NODE_PREFIX + 3 {
            return Err(StorageError::BpTreeNodeHeaderTruncated {
                need: NODE_PREFIX + 3,
                have: buf.len(),
            });
        }
        let tag = u64::from_le_bytes(buf[..NODE_PREFIX].try_into().unwrap());
        if tag != BPTREE_NODE_STRUCT_HASH {
            return Err(StorageError::BpTreeNodeBadTag(tag));
        }
        let kind = buf[NODE_PREFIX];
        let count =
            u16::from_le_bytes([buf[NODE_PREFIX + 1], buf[NODE_PREFIX + 2]])
                as usize;
        let mut p: usize = NODE_PREFIX + 3;
        let mut take = |n: usize| -> StorageResult<&[u8]> {
            // Saturation folds the (unreachable) offset-overflow case into the
            // same truncation fault: the saturated `end` always exceeds `len`.
            let end = p.saturating_add(n);
            if end > buf.len() {
                return Err(StorageError::BpTreeNodeTruncated {
                    need: end,
                    have: buf.len(),
                });
            }
            let s = &buf[p..end];
            p = end;
            Ok(s)
        };
        match kind {
            KIND_LEAF => {
                let mut keys = Vec::with_capacity(count);
                for _ in 0..count {
                    keys.push(read_local_id(take(LOCALID_LEN)?)?);
                }
                Ok(Self::Leaf(keys))
            }
            KIND_INTERNAL => {
                let leftmost = read_local_id(take(LOCALID_LEN)?)?;
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let sep = read_local_id(take(LOCALID_LEN)?)?;
                    let child = read_local_id(take(LOCALID_LEN)?)?;
                    entries.push((sep, child));
                }
                Ok(Self::Internal { leftmost, entries })
            }
            _ => Err(StorageError::BpTreeNodeBadKind(kind)),
        }
    }
}

/// Decode one 10-byte `LocalId`, lifting the wire fault through
/// [`StorageError::Core`] (the documented path for codec errors).
fn read_local_id(bytes: &[u8]) -> StorageResult<LocalId> {
    from_wire::<LocalId>(bytes).map_err(|e| StorageError::Core(e.into()))
}

/// Mint a fresh node `LocalId`: `CREATED_AT`-style key (nanos) with `FLAG = 1` to
/// namespace it away from records, and a per-process counter salt for uniqueness.
fn mint_node_id() -> LocalId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let salt = (NODE_SALT.fetch_add(1, Ordering::Relaxed) & 0x7FFF) as u16;
    LocalId::new(nanos, true, salt)
}

#[cfg(test)]
mod tests {
    use super::PageBpTree;
    use futures::StreamExt;
    use futures::executor::block_on;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use wavedb_core::{Bound, Id, Result, Store, U48, Write};

    /// In-memory `Store` for tree tests.
    #[derive(Default)]
    struct MemStore(Mutex<BTreeMap<u128, Vec<u8>>>);

    impl Store for MemStore {
        async fn get(&self, id: Id) -> Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(&id.raw()).cloned())
        }
        async fn apply(&self, batch: &[Write]) -> Result<()> {
            let mut m = self.0.lock().unwrap();
            for w in batch {
                match w {
                    Write::Put(id, b) => {
                        m.insert(id.raw(), b.clone());
                    }
                    Write::Remove(id) => {
                        m.remove(&id.raw());
                    }
                }
            }
            drop(m);
            Ok(())
        }
    }

    const TENANT: u32 = 7;

    fn rec(created_at: u64) -> Id {
        Id::new(
            created_at,
            U48::from(TENANT),
            false,
            (created_at & 0x7FFF) as u16,
        )
    }

    async fn all_keys(tree: &PageBpTree, store: &MemStore) -> Vec<u64> {
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
                PageBpTree::create(&store, U48::from(TENANT)).await.unwrap();
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
                PageBpTree::create(&store, U48::from(TENANT)).await.unwrap();
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
            let mut tree =
                PageBpTree::create(&store, U48::from(TENANT)).await.unwrap();
            let root0 = tree.root();
            // Enough distinct keys to force several leaf splits and at least one
            // internal split (LEAF_CAP = 1819), inserted in scrambled order.
            let mut expected: Vec<u64> = Vec::new();
            for i in 0..5000u64 {
                let k = i.wrapping_mul(2_654_435_761) % 1_000_003;
                tree.insert(&store, rec(k)).await.unwrap();
                expected.push(k);
            }
            expected.sort_unstable();
            expected.dedup();

            let got = all_keys(&tree, &store).await;
            assert!(got.windows(2).all(|w| w[0] < w[1]), "not strictly sorted");
            assert_eq!(
                got, expected,
                "tree lost or duplicated keys across splits"
            );
            assert_ne!(
                tree.root(),
                root0,
                "root never moved — tree did not grow"
            );
        });
    }

    #[test]
    fn range_search_filters_by_created_at() {
        block_on(async {
            let store = MemStore::default();
            let mut tree =
                PageBpTree::create(&store, U48::from(TENANT)).await.unwrap();
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
    fn remove_takes_key_out() {
        block_on(async {
            let store = MemStore::default();
            let mut tree =
                PageBpTree::create(&store, U48::from(TENANT)).await.unwrap();
            for k in [1u64, 2, 3, 4, 5] {
                tree.insert(&store, rec(k)).await.unwrap();
            }
            assert!(tree.remove(&store, rec(3)).await.unwrap());
            assert!(!tree.remove(&store, rec(3)).await.unwrap()); // gone already
            assert_eq!(all_keys(&tree, &store).await, vec![1, 2, 4, 5]);
        });
    }

    #[test]
    fn empty_tree_searches_nothing() {
        block_on(async {
            let store = MemStore::default();
            let tree =
                PageBpTree::create(&store, U48::from(TENANT)).await.unwrap();
            assert!(all_keys(&tree, &store).await.is_empty());
        });
    }
}
