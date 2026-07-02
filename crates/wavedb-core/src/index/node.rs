//! The `BpTree` node value: its `WaveWire` body, the reserved page-kind tag, and
//! node-id minting.
//!
//! ## Value format
//!
//! ```text
//! [ BPTREE_NODE_STRUCT_HASH (8 B LE) ][ WaveWire(NodeBody) ]
//! ```
//!
//! The leading 8 bytes are the raw page-kind tag — storage backends route every
//! stored value by the `STRUCT_HASH` in its first 8 bytes, and a node is an
//! ordinary value to any [`Store`](crate::store::Store). The body itself is
//! plain `WaveWire` (the workspace's one layout language): a canonical-form
//! enum, `Leaf` holding the sorted record keys, `Internal` holding the leftmost
//! child plus `(separator, child)` pairs.
//!
//! ## Node identity
//!
//! Node ids are minted with `FLAG = 1`, which namespaces them away from the
//! `FLAG = 0` record keys sharing the tenant's keyspace — a node id and a
//! record id can never collide. Minting is non-deterministic (clock + counter),
//! which is fine: nodes are persisted by value, so a journal replay reproduces
//! the exact bytes without re-minting.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::local_id::LocalId;
use crate::wire::{WaveWire, from_wire, to_wire};

/// Reserved `STRUCT_HASH` stamped at the head of every BpTree node's bytes.
///
/// Routes all nodes into one reserved storage directory. (A real struct hashing
/// to this exact value is a 2⁻⁶⁴ event and merely shares that directory,
/// harmlessly.)
pub const BPTREE_NODE_STRUCT_HASH: u64 = 0x42_50_54_52_45_45_00_01; // "BPTREE\0\x01"

/// Bytes before the wire body: the reserved `STRUCT_HASH` tag.
const NODE_PREFIX: usize = 8;

/// Process-wide counter salting node ids, so two nodes minted in the same
/// nanosecond still get distinct `LocalId`s.
static NODE_SALT: AtomicU64 = AtomicU64::new(0);

/// A B+tree node, decoded from its `Store` value.
///
/// An internal node with `n` separators has `n + 1` children: `leftmost` covers
/// keys below `entries[0].0`, then one child per separator covering keys `≥`
/// that separator.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub(super) enum NodeBody {
    /// Sorted record keys.
    Leaf(Vec<LocalId>),
    /// `leftmost` child plus ascending `(separator, child)` pairs.
    Internal {
        leftmost: LocalId,
        entries: Vec<(LocalId, LocalId)>,
    },
}

impl NodeBody {
    /// Serialise: the reserved tag, then the `WaveWire` body.
    pub(super) fn to_bytes(&self) -> Vec<u8> {
        let mut out = BPTREE_NODE_STRUCT_HASH.to_le_bytes().to_vec();
        out.extend_from_slice(&to_wire(self));
        out
    }

    /// Parse a node value, checking the reserved tag first.
    ///
    /// # Errors
    /// [`Error::BpTreeNodeBadTag`] if the value's first 8 bytes are not the
    /// reserved node tag (or the value is shorter than the tag);
    /// [`Error::Wire`] if the body fails to decode.
    pub(super) fn from_bytes(buf: &[u8]) -> Result<Self> {
        let tag_bytes: [u8; NODE_PREFIX] = buf
            .get(..NODE_PREFIX)
            .and_then(|s| s.try_into().ok())
            .ok_or(Error::BpTreeNodeBadTag(0))?;
        let tag = u64::from_le_bytes(tag_bytes);
        if tag != BPTREE_NODE_STRUCT_HASH {
            return Err(Error::BpTreeNodeBadTag(tag));
        }
        Ok(from_wire::<Self>(&buf[NODE_PREFIX..])?)
    }
}

/// Mint a fresh node `LocalId`: a nanosecond key with `FLAG = 1` (namespaced
/// away from records) and a per-process counter salt.
pub(super) fn mint_node_id() -> LocalId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let salt = (NODE_SALT.fetch_add(1, Ordering::Relaxed) & 0x7FFF) as u16;
    LocalId::new(nanos, true, salt)
}

#[cfg(test)]
mod tests {
    use super::{BPTREE_NODE_STRUCT_HASH, NodeBody, mint_node_id};
    use crate::error::Error;
    use crate::local_id::LocalId;

    fn lid(key: u64) -> LocalId {
        LocalId::new(key, false, 1)
    }

    #[test]
    fn leaf_roundtrips() {
        let n = NodeBody::Leaf(vec![lid(1), lid(2), lid(3)]);
        let bytes = n.to_bytes();
        assert_eq!(
            u64::from_le_bytes(bytes[..8].try_into().unwrap()),
            BPTREE_NODE_STRUCT_HASH,
            "node value must be STRUCT_HASH-headed"
        );
        assert_eq!(NodeBody::from_bytes(&bytes).unwrap(), n);
    }

    #[test]
    fn internal_roundtrips() {
        let n = NodeBody::Internal {
            leftmost: lid(10),
            entries: vec![(lid(20), lid(21)), (lid(30), lid(31))],
        };
        assert_eq!(NodeBody::from_bytes(&n.to_bytes()).unwrap(), n);
    }

    #[test]
    fn bad_tag_is_rejected_with_the_tag() {
        let mut bytes = NodeBody::Leaf(vec![]).to_bytes();
        bytes[0] ^= 0xFF;
        let err = NodeBody::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, Error::BpTreeNodeBadTag(t) if t != 0));
        // Shorter than the tag itself.
        assert!(matches!(
            NodeBody::from_bytes(&[1, 2, 3]),
            Err(Error::BpTreeNodeBadTag(0))
        ));
    }

    #[test]
    fn truncated_body_is_a_wire_fault() {
        let bytes = NodeBody::Leaf(vec![lid(1)]).to_bytes();
        let cut = &bytes[..bytes.len() - 3];
        assert!(matches!(NodeBody::from_bytes(cut), Err(Error::Wire(_))));
    }

    #[test]
    fn minted_ids_are_distinct_and_flagged() {
        let a = mint_node_id();
        let b = mint_node_id();
        assert_ne!(a, b);
        assert!(a.flag(), "node ids are FLAG = 1 (namespaced from records)");
    }
}
