//! The `Store`-generic index layer.
//!
//! Order-preserving [`IndexKey`] encoding, the [`Bound`] search range, the
//! [`Pivot`] roots holder, the concrete [`BpTree`], and [`IdStreamExt`] set
//! algebra over `Id` streams.
//!
//! Everything here depends only on [`Store`] (`get` + `apply`), so the same code
//! compiles for the node (`PageStore` in `wavedb-storage`) and the browser
//! (IndexedDB). Pages, blocks, and the journal are backend internals and are
//! never named here.

mod key;
#[cfg(test)]
pub(crate) mod mem_store;
mod node;
mod stream;
mod tree;
mod tree_delete;
mod tree_insert;

pub use key::IndexKey;
pub use node::BPTREE_NODE_STRUCT_HASH;
pub use stream::{Except, IdStreamExt, Intersect, Union};
pub use tree::{BpTree, DEFAULT_INTERNAL_CAP, DEFAULT_LEAF_CAP};

use crate::local_id::LocalId;
use crate::permission::PermissionRef;
use crate::wire::WaveWire;

// ---- Bound: a search range over the encoded key space -----------------------

/// A search bound over the order-preserving key space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bound {
    /// Every key in the tree.
    All,
    /// Keys byte-equal to this encoding.
    Exact(Vec<u8>),
    /// Half-open `[lo, hi)`.
    Range { lo: Vec<u8>, hi: Vec<u8> },
    /// Keys that start with this byte prefix.
    Prefix(Vec<u8>),
}

impl Bound {
    /// Does an encoded key fall within this bound? (`memcmp` semantics.)
    #[must_use]
    pub fn contains(&self, key: &[u8]) -> bool {
        match self {
            Self::All => true,
            Self::Exact(k) => key == k.as_slice(),
            Self::Range { lo, hi } => {
                key >= lo.as_slice() && key < hi.as_slice()
            }
            Self::Prefix(p) => key.starts_with(p),
        }
    }

    /// The inclusive `CREATED_AT` (`u64`) range this bound can match, when it is
    /// expressible — the tree's descent pruning. `None` = no pruning possible
    /// ([`Bound::All`], or a key that isn't an 8-byte big-endian `CREATED_AT`).
    /// A returned `(lo, hi)` with `lo > hi` matches nothing.
    #[must_use]
    pub(crate) fn created_at_range(&self) -> Option<(u64, u64)> {
        let as_u64 = |b: &[u8]| -> Option<u64> {
            Some(u64::from_be_bytes(b.try_into().ok()?))
        };
        match self {
            Self::All => None,
            Self::Exact(k) => {
                let k = as_u64(k)?;
                Some((k, k))
            }
            Self::Range { lo, hi } => {
                let (lo, hi) = (as_u64(lo)?, as_u64(hi)?);
                // Half-open [lo, hi) → inclusive. hi == 0 matches nothing;
                // wrapping would turn it into the full range, so signal the
                // empty range explicitly with lo > hi.
                if hi == 0 {
                    return Some((1, 0));
                }
                Some((lo, hi - 1))
            }
            Self::Prefix(p) => {
                if p.len() > 8 {
                    return None;
                }
                let mut lo = [0x00u8; 8];
                let mut hi = [0xFFu8; 8];
                lo[..p.len()].copy_from_slice(p);
                hi[..p.len()].copy_from_slice(p);
                Some((u64::from_be_bytes(lo), u64::from_be_bytes(hi)))
            }
        }
    }
}

// ---- Pivot: the collection's roots holder -----------------------------------

/// The collection's roots holder.
///
/// `#[wavedb]` generates one per NonUnique type; this trait is the portable shape
/// the engine reads. Root pointers are [`LocalId`] (tenant-scoped tree ⇒ `TENANT`
/// derivable). No element counter — the `Pivot` is rewritten only when a `BpTree`
/// root moves or its default permission changes (a rare admin op).
pub trait Pivot: WaveWire + Sized {
    /// Identity stamped at the head of the stored pivot record (`[STRUCT_HASH]
    /// [wire]`), routing all pivots of one collection type into one storage
    /// directory. The macro derives it from the generated pivot's own shape.
    const STRUCT_HASH: u64;

    /// Root of the living-records B+tree.
    fn current(&self) -> LocalId;
    /// Root of the deleted-records B+tree.
    fn dead(&self) -> LocalId;
    /// One root per `#[wavedb::pivot(...)]` secondary index.
    fn secondaries(&self) -> &[LocalId];
    /// Collection-default access rule: seeds new inserts and gates
    /// collection-scope ops (`Insert`, `All`). Each record's
    /// `Metadata.permission` overrides it (authoritative per record).
    /// `None` = tenant-only.
    fn permission(&self) -> Option<&PermissionRef>;
    /// A copy of this pivot with the `current` / `dead` roots replaced and
    /// everything else (secondaries, permission) preserved — what the engine
    /// writes back when a B+tree root moves.
    #[must_use]
    fn replace_roots(&self, current: LocalId, dead: LocalId) -> Self;
}

#[cfg(test)]
mod tests {
    use super::Bound;

    #[test]
    fn bound_contains() {
        assert!(Bound::All.contains(&[1, 2, 3]));
        assert!(Bound::Exact(vec![1, 2]).contains(&[1, 2]));
        assert!(!Bound::Exact(vec![1, 2]).contains(&[1, 3]));
        let r = Bound::Range {
            lo: vec![1],
            hi: vec![3],
        };
        assert!(r.contains(&[1]));
        assert!(r.contains(&[2]));
        assert!(!r.contains(&[3])); // half-open
        assert!(Bound::Prefix(vec![0xAB]).contains(&[0xAB, 0xCD]));
        assert!(!Bound::Prefix(vec![0xAB]).contains(&[0xAC]));
    }

    #[test]
    fn created_at_range_matches_contains_semantics() {
        let key = |v: u64| v.to_be_bytes().to_vec();

        assert_eq!(Bound::All.created_at_range(), None);
        assert_eq!(Bound::Exact(key(9)).created_at_range(), Some((9, 9)));
        // Non-8-byte keys carry no CREATED_AT meaning — no pruning.
        assert_eq!(Bound::Exact(vec![1, 2]).created_at_range(), None);

        let r = Bound::Range {
            lo: key(10),
            hi: key(20),
        };
        assert_eq!(r.created_at_range(), Some((10, 19))); // half-open → inclusive

        // hi == 0 matches nothing: lo > hi signals the empty range.
        let empty = Bound::Range {
            lo: key(0),
            hi: key(0),
        };
        let (lo, hi) = empty.created_at_range().unwrap();
        assert!(lo > hi);

        // A prefix covers the padded [p00.., pFF..] block.
        let p = Bound::Prefix(vec![0xAB]);
        assert_eq!(
            p.created_at_range(),
            Some((0xAB00_0000_0000_0000, 0xABFF_FFFF_FFFF_FFFF))
        );
    }
}
