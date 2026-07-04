//! [`NodeKey`] — the key type a [`BpTree`](super::BpTree) is generic over —
//! and [`SecKey`], the secondary-index key.
//!
//! The primary tree keys by [`LocalId`] (order = `CREATED_AT`; the key **is**
//! the record pointer). A secondary index keys by the record's
//! [`IndexKey`](super::IndexKey)-encoded field bytes plus its `LocalId` — the
//! trailing pointer makes entries unique when many records share one field
//! value, and doubles as the leaf payload. Both are ordinary `Ord + WaveWire`
//! values, so the same tree machinery serves both, fully monomorphized.

use crate::local_id::LocalId;
use crate::wire::WaveWire;

use super::Bound;

/// A `BpTree` key: ordered, wire-encodable, and search-aware.
///
/// Beyond `Ord + WaveWire`, a key answers the search walk's two questions —
/// does a leaf key match a [`Bound`], and can a subtree window possibly
/// contain a match (descent pruning; a conservative `true` only costs
/// pruning, never correctness).
pub trait NodeKey: Clone + Ord + core::fmt::Debug + WaveWire {
    /// The record this leaf entry points at.
    fn record(&self) -> LocalId;

    /// Does this leaf key satisfy `bound`?
    fn matches(&self, bound: &Bound) -> bool;

    /// May any key in the inclusive window `[min, max]` (`None` = unbounded)
    /// satisfy `bound`?
    fn may_intersect(
        bound: &Bound,
        min: Option<&Self>,
        max: Option<&Self>,
    ) -> bool;
}

/// The primary key: the record's `LocalId` itself.
///
/// Bounds carry the 8-byte big-endian `CREATED_AT`; pruning compares the
/// window's key range against [`Bound::created_at_range`].
impl NodeKey for LocalId {
    fn record(&self) -> LocalId {
        *self
    }

    fn matches(&self, bound: &Bound) -> bool {
        match bound {
            Bound::All => true,
            _ => bound.contains(&self.key().to_be_bytes()),
        }
    }

    fn may_intersect(
        bound: &Bound,
        min: Option<&Self>,
        max: Option<&Self>,
    ) -> bool {
        bound.created_at_range().is_none_or(|(lo, hi)| {
            let win_min = min.map_or(0, |k| k.key());
            let win_max = max.map_or(u64::MAX, |k| k.key());
            win_max >= lo && win_min <= hi
        })
    }
}

/// A secondary-index key: the record's order-preserving field encoding, then
/// its `LocalId`.
///
/// Derived `Ord` is field-major (ties broken by the record), so equal field
/// values coexist and range scans stay contiguous.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, WaveWire)]
pub struct SecKey {
    /// The [`IndexKey`](super::IndexKey)-encoded field value(s).
    pub field: Vec<u8>,
    /// The indexed record (also the leaf payload).
    pub rec: LocalId,
}

impl NodeKey for SecKey {
    fn record(&self) -> LocalId {
        self.rec
    }

    fn matches(&self, bound: &Bound) -> bool {
        bound.contains(&self.field)
    }

    fn may_intersect(
        bound: &Bound,
        min: Option<&Self>,
        max: Option<&Self>,
    ) -> bool {
        // Field bytes are the key's major component, so the window's field
        // range is [min.field, max.field] (inclusive, conservative).
        let lo = min.map(|k| k.field.as_slice());
        let hi = max.map(|k| k.field.as_slice());
        match bound {
            Bound::All => true,
            Bound::Exact(k) => {
                hi.is_none_or(|h| h >= k.as_slice())
                    && lo.is_none_or(|l| l <= k.as_slice())
            }
            Bound::Range { lo: b_lo, hi: b_hi } => {
                hi.is_none_or(|h| h >= b_lo.as_slice())
                    && lo.is_none_or(|l| l < b_hi.as_slice())
            }
            Bound::Prefix(p) => {
                hi.is_none_or(|h| h >= p.as_slice())
                    && lo.is_none_or(|l| le_prefix_max(l, p))
            }
        }
    }
}

/// Is `x` ≤ the greatest key carrying prefix `p` (conceptually `p` extended
/// with `0xFF…`)? True at the first differing byte with `x < p`, and whenever
/// one is a prefix of the other (inside or below the region).
fn le_prefix_max(x: &[u8], p: &[u8]) -> bool {
    for (a, b) in x.iter().zip(p) {
        if a != b {
            return a < b;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::Bound;
    use super::{NodeKey, SecKey};
    use crate::local_id::LocalId;

    fn sec(field: &[u8], key: u64) -> SecKey {
        SecKey {
            field: field.to_vec(),
            rec: LocalId::new(key, false, 1),
        }
    }

    #[test]
    fn sec_key_orders_field_major_then_record() {
        assert!(sec(b"a", 9) < sec(b"b", 1));
        assert!(sec(b"a", 1) < sec(b"a", 2));
        assert!(sec(b"a", 1) < sec(b"ab", 0), "prefix sorts first");
    }

    #[test]
    fn sec_key_matches_field_bounds() {
        let k = sec(b"apple", 7);
        assert!(k.matches(&Bound::All));
        assert!(k.matches(&Bound::Exact(b"apple".to_vec())));
        assert!(!k.matches(&Bound::Exact(b"app".to_vec())));
        assert!(k.matches(&Bound::Prefix(b"app".to_vec())));
        assert!(k.matches(&Bound::Range {
            lo: b"a".to_vec(),
            hi: b"b".to_vec(),
        }));
    }

    #[test]
    fn sec_window_pruning_is_conservative_and_correct() {
        let w = |lo: &[u8], hi: &[u8]| (sec(lo, 0), sec(hi, 0));
        let (lo, hi) = w(b"c", b"f");
        let hit = |b: &Bound| SecKey::may_intersect(b, Some(&lo), Some(&hi));

        assert!(hit(&Bound::All));
        assert!(hit(&Bound::Exact(b"d".to_vec())));
        assert!(hit(&Bound::Exact(b"c".to_vec()))); // window edge
        assert!(!hit(&Bound::Exact(b"a".to_vec())));
        assert!(!hit(&Bound::Exact(b"g".to_vec())));
        assert!(hit(&Bound::Range {
            lo: b"e".to_vec(),
            hi: b"z".to_vec(),
        }));
        assert!(!hit(&Bound::Range {
            lo: b"g".to_vec(),
            hi: b"z".to_vec(),
        }));
        assert!(hit(&Bound::Prefix(b"d".to_vec())));
        assert!(!hit(&Bound::Prefix(b"a".to_vec())));
        assert!(!hit(&Bound::Prefix(b"g".to_vec())));
        // Unbounded edges always stay in play.
        assert!(SecKey::may_intersect(
            &Bound::Exact(b"zzz".to_vec()),
            Some(&lo),
            None
        ));
    }
}
