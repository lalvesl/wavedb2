//! The `BpTree` node value: its byte form, the reserved page-kind tag, and
//! node-id minting.
//!
//! ## Value format
//!
//! ```text
//! [ BPTREE_NODE_STRUCT_HASH (8 B LE) ][ kind (u8) ][ WaveWire payload ]
//! ```
//!
//! The leading 8 bytes are the raw page-kind tag — storage backends route every
//! stored value by the `STRUCT_HASH` in its first 8 bytes, and a node is an
//! ordinary value to any [`Store`](crate::store::Store). `kind` picks the
//! variant; the payload is plain `WaveWire` (the workspace's one layout
//! language) composed from the generic `Vec` / tuple impls: a leaf is
//! `Vec<K>`, an internal node is `LocalId` (the leftmost child, fixed
//! [`STACK_SIZE`](WaveWire::STACK_SIZE)) followed by `Vec<(K, LocalId)>`.
//! Primary ([`LocalId`]) and secondary ([`SecKey`](super::SecKey)) nodes share
//! the tag: the tree opening a root knows its own key type.
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

use super::node_key::NodeKey;

/// Reserved `STRUCT_HASH` stamped at the head of every BpTree node's bytes.
///
/// Routes all nodes into one reserved storage directory. (A real struct hashing
/// to this exact value is a 2⁻⁶⁴ event and merely shares that directory,
/// harmlessly.)
pub const BPTREE_NODE_STRUCT_HASH: u64 = 0x42_50_54_52_45_45_00_01; // "BPTREE\0\x01"

/// Bytes before the kind byte: the reserved `STRUCT_HASH` tag.
const NODE_PREFIX: usize = 8;

/// `kind` byte values.
const KIND_LEAF: u8 = 0;
const KIND_INTERNAL: u8 = 1;

/// Process-wide counter salting node ids, so two nodes minted in the same
/// nanosecond still get distinct `LocalId`s.
static NODE_SALT: AtomicU64 = AtomicU64::new(0);

/// A B+tree node, decoded from its `Store` value.
///
/// An internal node with `n` separators has `n + 1` children: `leftmost` covers
/// keys below `entries[0].0`, then one child per separator covering keys `≥`
/// that separator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NodeBody<K: NodeKey> {
    /// Sorted keys.
    Leaf(Vec<K>),
    /// `leftmost` child plus ascending `(separator, child)` pairs.
    Internal {
        leftmost: LocalId,
        entries: Vec<(K, LocalId)>,
    },
}

impl<K: NodeKey> NodeBody<K> {
    /// Serialise: the reserved tag, the kind byte, then the `WaveWire` payload.
    pub(super) fn to_bytes(&self) -> Vec<u8> {
        let mut out = BPTREE_NODE_STRUCT_HASH.to_le_bytes().to_vec();
        match self {
            Self::Leaf(keys) => {
                out.push(KIND_LEAF);
                out.extend_from_slice(&to_wire(keys));
            }
            Self::Internal { leftmost, entries } => {
                out.push(KIND_INTERNAL);
                out.extend_from_slice(&to_wire(leftmost));
                out.extend_from_slice(&to_wire(entries));
            }
        }
        out
    }

    /// Parse a node value, checking the reserved tag first.
    ///
    /// # Errors
    /// [`Error::BpTreeNodeBadTag`] if the value's first 8 bytes are not the
    /// reserved node tag (or the value is shorter than the tag + kind, or the
    /// kind byte is unknown); [`Error::Wire`] if the payload fails to decode.
    pub(super) fn from_bytes(buf: &[u8]) -> Result<Self> {
        let tag_bytes: [u8; NODE_PREFIX] = buf
            .get(..NODE_PREFIX)
            .and_then(|s| s.try_into().ok())
            .ok_or(Error::BpTreeNodeBadTag(0))?;
        let tag = u64::from_le_bytes(tag_bytes);
        if tag != BPTREE_NODE_STRUCT_HASH {
            return Err(Error::BpTreeNodeBadTag(tag));
        }
        let kind = *buf.get(NODE_PREFIX).ok_or(Error::BpTreeNodeBadTag(tag))?;
        let payload = &buf[NODE_PREFIX + 1..];
        match kind {
            KIND_LEAF => Ok(Self::Leaf(from_wire::<Vec<K>>(payload)?)),
            KIND_INTERNAL => {
                // `LocalId` is stack-only (fixed size, no heap), so the
                // leftmost child ends exactly at its STACK_SIZE.
                let split = <LocalId as WaveWire>::STACK_SIZE;
                if payload.len() < split {
                    return Err(Error::Wire(wavedb_wire::Error::UnexpectedEof));
                }
                let leftmost = from_wire::<LocalId>(&payload[..split])?;
                let entries =
                    from_wire::<Vec<(K, LocalId)>>(&payload[split..])?;
                Ok(Self::Internal { leftmost, entries })
            }
            _ => Err(Error::BpTreeNodeBadTag(tag)),
        }
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
    use super::super::node_key::SecKey;
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
    fn secondary_key_nodes_roundtrip() {
        let key = |f: &[u8], k: u64| SecKey {
            field: f.to_vec(),
            rec: lid(k),
        };
        let leaf = NodeBody::Leaf(vec![key(b"a", 1), key(b"bb", 2)]);
        assert_eq!(NodeBody::from_bytes(&leaf.to_bytes()).unwrap(), leaf);

        let internal = NodeBody::Internal {
            leftmost: lid(9),
            entries: vec![(key(b"m", 5), lid(6))],
        };
        assert_eq!(
            NodeBody::from_bytes(&internal.to_bytes()).unwrap(),
            internal
        );
    }

    #[test]
    fn bad_tag_is_rejected_with_the_tag() {
        let mut bytes = NodeBody::<LocalId>::Leaf(vec![]).to_bytes();
        bytes[0] ^= 0xFF;
        let err = NodeBody::<LocalId>::from_bytes(&bytes).unwrap_err();
        assert!(matches!(err, Error::BpTreeNodeBadTag(t) if t != 0));
        // Shorter than the tag itself.
        assert!(matches!(
            NodeBody::<LocalId>::from_bytes(&[1, 2, 3]),
            Err(Error::BpTreeNodeBadTag(0))
        ));
    }

    #[test]
    fn truncated_body_is_a_wire_fault() {
        let bytes = NodeBody::Leaf(vec![lid(1)]).to_bytes();
        let cut = &bytes[..bytes.len() - 3];
        assert!(matches!(
            NodeBody::<LocalId>::from_bytes(cut),
            Err(Error::Wire(_))
        ));
    }

    #[test]
    fn minted_ids_are_distinct_and_flagged() {
        let a = mint_node_id();
        let b = mint_node_id();
        assert_ne!(a, b);
        assert!(a.flag(), "node ids are FLAG = 1 (namespaced from records)");
    }
}
