//! The `Store`-generic index contracts.
//!
//! Order-preserving [`IndexKey`] encoding, the [`Bound`] search range, the
//! [`Pivot`] roots holder, the [`BpTree`] trait, and [`IdStreamExt`] set algebra
//! over `Id` streams.
//!
//! Everything here depends only on [`Store`] (`get` + `apply`), so the same code
//! compiles for the node (`PageStore`) and the browser (IndexedDB). Pages, blocks,
//! and the journal are `PageStore` internals and are never named here.

use std::collections::HashSet;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::Stream;

use crate::error::Result;
use crate::id::Id;
use crate::local_id::LocalId;
use crate::permission::PermissionRef;
use crate::store::Store;
use crate::u48::U48;
use crate::wire::WaveWire;

// ---- IndexKey: order-preserving encoding ------------------------------------

/// Encode a value so that **byte order equals value order** — the `BpTree`
/// compares keys with `memcmp` and never decodes them.
///
/// Implemented per indexed type by the macro: unsigned ints big-endian, signed
/// ints sign-flipped then big-endian, `String` `0x00`-terminated, tuples
/// concatenated in declaration order.
pub trait IndexKey {
    /// Append this value's order-preserving key bytes to `out`.
    fn encode_key(&self, out: &mut Vec<u8>);

    /// Convenience: encode into a fresh `Vec`.
    #[must_use]
    fn key_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_key(&mut out);
        out
    }
}

macro_rules! index_key_unsigned {
    ($($t:ty),*) => {$(
        impl IndexKey for $t {
            fn encode_key(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_be_bytes());
            }
        }
    )*};
}
index_key_unsigned!(u8, u16, u32, u64, u128);

macro_rules! index_key_signed {
    ($($t:ty => $u:ty),*) => {$(
        impl IndexKey for $t {
            fn encode_key(&self, out: &mut Vec<u8>) {
                // Flip the sign bit so negatives sort before positives in BE order.
                let bias: $u = (1 as $u) << (<$u>::BITS - 1);
                out.extend_from_slice(&((*self as $u) ^ bias).to_be_bytes());
            }
        }
    )*};
}
index_key_signed!(i8 => u8, i16 => u16, i32 => u32, i64 => u64, i128 => u128);

impl IndexKey for bool {
    fn encode_key(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }
}

impl IndexKey for char {
    fn encode_key(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(*self as u32).to_be_bytes());
    }
}

impl IndexKey for U48 {
    fn encode_key(&self, out: &mut Vec<u8>) {
        // Low 48 bits, big-endian (drop the two high zero bytes of the u64).
        out.extend_from_slice(&self.get().to_be_bytes()[2..]);
    }
}

impl IndexKey for LocalId {
    fn encode_key(&self, out: &mut Vec<u8>) {
        // KEY (8 B BE) then FLAG|SALT (2 B BE) — matches the field priority of Ord.
        out.extend_from_slice(&self.key().to_be_bytes());
        let lower = (u16::from(self.flag()) << 15) | self.salt();
        out.extend_from_slice(&lower.to_be_bytes());
    }
}

impl IndexKey for str {
    fn encode_key(&self, out: &mut Vec<u8>) {
        // 0x00-terminated: a proper prefix sorts before its extension.
        out.extend_from_slice(self.as_bytes());
        out.push(0);
    }
}

impl IndexKey for String {
    fn encode_key(&self, out: &mut Vec<u8>) {
        self.as_str().encode_key(out);
    }
}

impl<T: IndexKey + ?Sized> IndexKey for &T {
    fn encode_key(&self, out: &mut Vec<u8>) {
        (**self).encode_key(out);
    }
}

macro_rules! index_key_tuple {
    ($($name:ident $idx:tt),+) => {
        impl<$($name: IndexKey),+> IndexKey for ($($name,)+) {
            fn encode_key(&self, out: &mut Vec<u8>) {
                $(self.$idx.encode_key(out);)+
            }
        }
    };
}
index_key_tuple!(A 0, B 1);
index_key_tuple!(A 0, B 1, C 2);

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
}

// ---- Pivot: the collection's roots holder -----------------------------------

/// The collection's roots holder.
///
/// `#[wavedb]` generates one per NonUnique type; this trait is the portable shape
/// the engine reads. Root pointers are [`LocalId`] (tenant-scoped tree ⇒ `TENANT`
/// derivable). No element counter — the `Pivot` is rewritten only when a `BpTree`
/// root moves or its default permission changes (a rare admin op).
pub trait Pivot: WaveWire + Sized {
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
}

// ---- BpTree: the index over any Store ---------------------------------------

/// A B+tree index over any [`Store`].
///
/// Nodes are records read by [`LocalId`]; all I/O is delegated to `Store`, so one
/// impl serves native `PageStore` and web IndexedDB. `search` returns full record
/// [`Id`]s (two-phase: index → `Id`s → fetch); `insert`/`remove` take a record
/// `Id` and return the (possibly moved) root as a `LocalId`.
pub trait BpTree<S: Store>: Sized {
    /// Open a tree at a root pointer.
    fn at(root: LocalId) -> Self;

    /// Walk the tree, yielding matching record `Id`s in key order.
    fn search(&self, store: &S, bound: Bound)
    -> impl Stream<Item = Result<Id>>;

    /// Insert `id` under `key`; returns the (possibly moved) root.
    async fn insert(&self, store: &S, key: &[u8], id: Id) -> Result<LocalId>;

    /// Remove `id` under `key`; returns the (possibly moved) root.
    async fn remove(&self, store: &S, key: &[u8], id: Id) -> Result<LocalId>;
}

// ---- IdStreamExt: set algebra over Id streams -------------------------------

/// Set algebra over the `Id` streams that [`BpTree::search`] yields.
///
/// The no-DSL composite query. `Store`-agnostic, so it runs native or web. Streams
/// from different indexes arrive in different orders, so `intersect`/`except`
/// buffer the argument side into an `Id` set and probe the receiver; `union`
/// merges and dedups.
pub trait IdStreamExt: Stream<Item = Result<Id>> + Sized + Unpin {
    /// Ids present in **both** streams (AND).
    fn intersect<B>(self, other: B) -> Intersect<Self, B>
    where
        B: Stream<Item = Result<Id>> + Unpin,
    {
        Intersect {
            left: self,
            right: other,
            buffered: None,
        }
    }

    /// Ids present in **either** stream, deduplicated (OR).
    fn union<B>(self, other: B) -> Union<Self, B>
    where
        B: Stream<Item = Result<Id>> + Unpin,
    {
        Union {
            left: self,
            right: other,
            seen: HashSet::new(),
            left_done: false,
        }
    }

    /// Ids in the receiver but **not** in `other` (NOT / difference).
    fn except<B>(self, other: B) -> Except<Self, B>
    where
        B: Stream<Item = Result<Id>> + Unpin,
    {
        Except {
            left: self,
            right: other,
            buffered: None,
        }
    }
}

impl<T: Stream<Item = Result<Id>> + Sized + Unpin> IdStreamExt for T {}

/// Drive `s` to completion, collecting raw ids into `set`; propagate errors and
/// pending. Returns `Poll::Ready(Ok(()))` once the stream is exhausted.
fn drain_into<S>(
    s: &mut S,
    set: &mut HashSet<u128>,
    cx: &mut Context<'_>,
) -> Poll<Result<()>>
where
    S: Stream<Item = Result<Id>> + Unpin,
{
    loop {
        match Pin::new(&mut *s).poll_next(cx) {
            Poll::Ready(Some(Ok(id))) => {
                set.insert(id.raw());
            }
            Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
            Poll::Ready(None) => return Poll::Ready(Ok(())),
            Poll::Pending => return Poll::Pending,
        }
    }
}

/// Intersection adapter — see [`IdStreamExt::intersect`].
pub struct Intersect<A, B> {
    left: A,
    right: B,
    buffered: Option<HashSet<u128>>,
}

impl<A, B> Stream for Intersect<A, B>
where
    A: Stream<Item = Result<Id>> + Unpin,
    B: Stream<Item = Result<Id>> + Unpin,
{
    type Item = Result<Id>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.buffered.is_none() {
            let mut set = HashSet::new();
            match drain_into(&mut this.right, &mut set, cx) {
                Poll::Ready(Ok(())) => this.buffered = Some(set),
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                Poll::Pending => return Poll::Pending,
            }
        }
        let set = this.buffered.as_ref().unwrap();
        loop {
            match Pin::new(&mut this.left).poll_next(cx) {
                Poll::Ready(Some(Ok(id))) => {
                    if set.contains(&id.raw()) {
                        return Poll::Ready(Some(Ok(id)));
                    }
                }
                other => return other,
            }
        }
    }
}

/// Difference adapter — see [`IdStreamExt::except`].
pub struct Except<A, B> {
    left: A,
    right: B,
    buffered: Option<HashSet<u128>>,
}

impl<A, B> Stream for Except<A, B>
where
    A: Stream<Item = Result<Id>> + Unpin,
    B: Stream<Item = Result<Id>> + Unpin,
{
    type Item = Result<Id>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.buffered.is_none() {
            let mut set = HashSet::new();
            match drain_into(&mut this.right, &mut set, cx) {
                Poll::Ready(Ok(())) => this.buffered = Some(set),
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                Poll::Pending => return Poll::Pending,
            }
        }
        let set = this.buffered.as_ref().unwrap();
        loop {
            match Pin::new(&mut this.left).poll_next(cx) {
                Poll::Ready(Some(Ok(id))) => {
                    if !set.contains(&id.raw()) {
                        return Poll::Ready(Some(Ok(id)));
                    }
                }
                other => return other,
            }
        }
    }
}

/// Union adapter — see [`IdStreamExt::union`]. Emits the left stream (recording
/// each id), then the right stream skipping ids already emitted.
pub struct Union<A, B> {
    left: A,
    right: B,
    seen: HashSet<u128>,
    left_done: bool,
}

impl<A, B> Stream for Union<A, B>
where
    A: Stream<Item = Result<Id>> + Unpin,
    B: Stream<Item = Result<Id>> + Unpin,
{
    type Item = Result<Id>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if !this.left_done {
            loop {
                match Pin::new(&mut this.left).poll_next(cx) {
                    Poll::Ready(Some(Ok(id))) => {
                        if this.seen.insert(id.raw()) {
                            return Poll::Ready(Some(Ok(id)));
                        }
                    }
                    Poll::Ready(Some(Err(e))) => {
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Ready(None) => {
                        this.left_done = true;
                        break;
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }
        }
        loop {
            match Pin::new(&mut this.right).poll_next(cx) {
                Poll::Ready(Some(Ok(id))) => {
                    if this.seen.insert(id.raw()) {
                        return Poll::Ready(Some(Ok(id)));
                    }
                }
                other => return other,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use futures::executor::block_on;
    use futures::stream;

    use super::{Bound, IdStreamExt, IndexKey};
    use crate::error::Result;
    use crate::id::Id;
    use crate::u48::U48;

    fn key_of<T: IndexKey>(v: T) -> Vec<u8> {
        v.key_bytes()
    }

    #[test]
    fn unsigned_keys_are_order_preserving() {
        let mut sorted = [0u64, 1, 255, 256, u64::MAX - 1, u64::MAX];
        sorted.sort_unstable();
        for w in sorted.windows(2) {
            assert!(key_of(w[0]) < key_of(w[1]), "{} vs {}", w[0], w[1]);
        }
    }

    #[test]
    fn signed_keys_sort_negatives_first() {
        let mut sorted = [i64::MIN, -2, -1, 0, 1, 2, i64::MAX];
        sorted.sort_unstable();
        for w in sorted.windows(2) {
            assert!(key_of(w[0]) < key_of(w[1]), "{} vs {}", w[0], w[1]);
        }
    }

    #[test]
    fn string_keys_prefix_sorts_first() {
        assert!(key_of("app".to_string()) < key_of("apple".to_string()));
        assert!(key_of("app".to_string()) < key_of("apq".to_string()));
        assert!(key_of("a".to_string()) < key_of("b".to_string()));
    }

    #[test]
    fn u48_and_tuple_keys() {
        assert!(key_of(U48::from(1u32)) < key_of(U48::from(2u32)));
        // Composite: primary field dominates, secondary breaks ties.
        assert!(key_of((1u32, 9u32)) < key_of((2u32, 0u32)));
        assert!(key_of((2u32, 0u32)) < key_of((2u32, 1u32)));
    }

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

    fn ids(keys: &[u64]) -> impl Stream<Item = Result<Id>> + Unpin {
        let v: Vec<Result<Id>> = keys
            .iter()
            .map(|&k| Ok(Id::new(k, U48::from(1u32), false, 0)))
            .collect();
        stream::iter(v)
    }

    fn keys_of(out: Vec<Result<Id>>) -> Vec<u64> {
        out.into_iter().map(|r| r.unwrap().key()).collect()
    }

    use futures::Stream;

    #[test]
    fn intersect_keeps_common() {
        block_on(async {
            let got = ids(&[1, 2, 3, 4])
                .intersect(ids(&[2, 4, 6]))
                .collect()
                .await;
            assert_eq!(keys_of(got), vec![2, 4]);
        });
    }

    #[test]
    fn union_dedups() {
        block_on(async {
            let got = ids(&[1, 2, 3]).union(ids(&[3, 4, 1])).collect().await;
            assert_eq!(keys_of(got), vec![1, 2, 3, 4]);
        });
    }

    #[test]
    fn except_removes_right() {
        block_on(async {
            let got = ids(&[1, 2, 3, 4]).except(ids(&[2, 4])).collect().await;
            assert_eq!(keys_of(got), vec![1, 3]);
        });
    }

    #[test]
    fn error_propagates() {
        block_on(async {
            let left = stream::iter(vec![
                Ok(Id::default()),
                Err(crate::error::Error::Wire(wavedb_wire::Error::Utf8)),
            ]);
            let right = ids(&[]);
            let got: Vec<Result<Id>> = left.intersect(right).collect().await;
            assert!(got.iter().any(Result::is_err));
        });
    }
}
