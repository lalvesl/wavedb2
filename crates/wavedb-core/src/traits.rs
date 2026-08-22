//! `WaveDbStruct` — the per-struct contract the `#[wavedb]` proc-macro implements,
//! and the [`Shape`] marker that records a type's cardinality discipline.
//!
//! Core only declares the surface; the macro fills in `STRUCT_HASH`, `SHAPE`, and
//! the generated `PivotId` for each declared object.

use crate::local_id::LocalId;
use crate::wire::WaveWire;

/// The shared surface of every generated `{Name}PivotId` — a [`LocalId`]
/// newtype an owning record stores to reference a collection.
///
/// Lets code that is generic over a record type reach its collection handle's
/// `LocalId` without naming the macro-generated concrete type (the typed
/// client `collection()` builds an insert/update payload from it).
pub trait PivotHandle: Copy {
    /// The underlying collection-root `LocalId`.
    fn local_id(self) -> LocalId;

    /// Wrap a `LocalId` back into the typed handle.
    fn from_local_id(local: LocalId) -> Self;
}

/// The cardinality discipline of a `#[wavedb]` object.
///
/// The shape is folded into the `STRUCT_HASH`, so two structs with the same name
/// and fields but different shapes are still distinct types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Shape {
    /// Exactly one live record per tenant, stored at a `STRUCT_HASH` anchor
    /// (`FLAG = 1`). The default for `#[wavedb]`.
    Unique,
    /// Many records per tenant, timestamp-keyed (`FLAG = 0`), reached through a
    /// [`Pivot`](crate::index::Pivot). May nest in other NonUnique collections.
    NonUnique,
}

impl Shape {
    /// `true` for [`Shape::Unique`].
    #[must_use]
    pub const fn is_unique(self) -> bool {
        matches!(self, Self::Unique)
    }

    /// The `FLAG` bit of this shape's **live anchor** ids: hash-keyed
    /// anchors (`Unique`) sit at `FLAG = 1`, time-keyed anchors
    /// (`NonUnique`) at `FLAG = 0`. Archives always take the **flipped**
    /// bit, so an archive can never collide with a live anchor — in
    /// particular a NonUnique first version, whose authoring instant *is*
    /// its anchor's key.
    #[must_use]
    pub const fn anchor_flag(self) -> bool {
        matches!(self, Self::Unique)
    }
}

/// Implemented by every `#[wavedb]` struct (by the proc-macro). The single source
/// of a type's compile-time identity, shape, and collection-handle type.
///
/// `STRUCT_HASH` is `seahash(name + shape + field names + field types)` with a
/// fixed seed. SeaHash is portable across architectures, so client and server
/// agree on identity; any schema change yields a new value.
pub trait WaveDbStruct: WaveWire {
    /// Compile-time identity of this type and its schema generation.
    const STRUCT_HASH: u64;

    /// This type's cardinality discipline.
    const SHAPE: Shape;

    /// The reserved lane hashes this type's storage occupies **beyond** its
    /// own records — one directory per kind of thing a collection stores: the
    /// **declared lists'** segments (the only ones holding records), the
    /// **recency** chain, the **removal log**, and the **sparse index** above
    /// them. Recency and `dead` are split from the record lane because they
    /// hold ids rather than records (RFC 0054), and a lane exists so one page
    /// and one zstd dictionary model one kind of content. A `Unique` type has
    /// no collection and so no lanes, which is the default.
    ///
    /// Derived by the macro at expansion time, because SeaHash is not a
    /// `const fn` (the same reason the `StructStorage` slots carry literals).
    /// They are listed here because a lane occupies a
    /// `type_salt` (the low 15 bits) exactly as a record type does,
    /// and the registry's collision guard has to see every occupant to
    /// uphold "a segment id can never equal a record anchor".
    const LANE_HASHES: &'static [u64] = &[];

    /// The typed handle into this type's collection.
    ///
    /// For a `NonUnique` type the macro generates a concrete `PivotId` (a wrapper
    /// over a [`LocalId`]) that callers store in a field
    /// to reference the collection. A `Unique` type has no collection, so its
    /// `PivotId` is `()`.
    type PivotId;
}

/// Implemented (by the proc-macro) for every default `#[wavedb]` (`Unique`)
/// struct — the compile-time counterpart to [`NonUniqueStruct`].
///
/// A `Unique` type has exactly one live record per tenant at its
/// `STRUCT_HASH` anchor. This marker lets a client's typed `get`/`save`
/// surface be gated to `Unique` types only (a `NonUnique` type is reached
/// through its collection instead), the mirror of how `NonUniqueStruct`
/// gates the collection surface — the two never overlap.
pub trait UniqueStruct: WaveDbStruct {}

/// Implemented (by the proc-macro) for every `#[wavedb(NonUnique)]` struct.
///
/// Ties the record type to its generated `{Name}Pivot` roots holder. This is
/// the bound [`Collection`](crate::collection::Collection) is generic over —
/// `Unique` types don't implement it, so a `Unique` type can never be driven
/// through a collection at compile time.
pub trait NonUniqueStruct: WaveDbStruct {
    /// The generated `{Name}Pivot` type holding this collection's roots.
    /// `Default` is the empty pivot [`Collection::create`] starts from.
    ///
    /// [`Collection::create`]: crate::collection::Collection::create
    type Pivot: crate::index::Pivot + Clone + Default;

    /// Number of `#[wavedb::pivot(...)]` secondary indexes, declaration order.
    /// Must equal the generated pivot's `secondaries()` length.
    const NUM_SECONDARIES: usize = 0;

    /// The record chain's segment capacity as a **minimum** — the developer's
    /// `#[wavedb(NonUnique, page = N)]`, defaulting to
    /// [`DEFAULT_SEGMENT_MIN`](crate::index::DEFAULT_SEGMENT_MIN).
    ///
    /// A segment holds `N…2N` records, splits 50/50 at `2N` and merges at
    /// `N/2` (RFC 0052), so a rendered page of N rows is one segment read —
    /// which is the whole reason the knob is spelled `page`. The cost lands on
    /// the write side: the chain is modification-ordered, so **every save
    /// rewrites the growth-end segment whole**, and that is `N…2N` records'
    /// bytes re-encoded per save.
    ///
    /// It folds into the [`STRUCT_HASH`](crate::WaveDbStruct::STRUCT_HASH),
    /// so a chain is only ever laid out at one capacity.
    const PAGE: usize = crate::index::DEFAULT_SEGMENT_MIN;

    /// Number of `#[wavedb::list(...)]` declared lists, declaration order.
    /// Must equal the generated pivot's `lists()` length.
    const NUM_LISTS: usize = 0;

    /// Declared list `index`'s segment capacity — its own `page = N`, or the
    /// struct's [`PAGE`](Self::PAGE) when it declared none.
    ///
    /// A per-list capacity exists because the two chain kinds have opposite
    /// write profiles. The built-in chain is modification-ordered, so **every**
    /// save rewrites its growth-end segment whole — which is why its default is
    /// deliberately small (16). A declared list is keyed by a domain value, so
    /// an ordinary save rewrites its entry *in place* in one segment; it can
    /// afford the N a rendered page actually wants without taxing every write.
    ///
    /// It folds into the [`STRUCT_HASH`](crate::WaveDbStruct::STRUCT_HASH) with
    /// the declaration it belongs to, so a list chain is only ever laid out at
    /// one capacity (RFC 0052).
    #[must_use]
    fn list_page(index: usize) -> usize {
        let _ = index;
        Self::PAGE
    }

    /// The order-preserving ([`IndexKey`](crate::index::IndexKey)-encoded) key
    /// of declared list `index` for this record's current values — the property
    /// that list is sorted by ([RFC 0051]).
    ///
    /// Same encoding as [`secondary_key`](Self::secondary_key), and deliberately
    /// so: a list and a secondary index impose the *same* order, and differ only
    /// in what they store under it (whole records inline versus anchors). The
    /// tie-break, however, is the **anchor** rather than the live version's
    /// authoring instant — the anchor is immutable, so a record moves inside a
    /// sorted chain only when the declared property itself changed, where the
    /// built-in chain moves it on every save by design.
    ///
    /// [RFC 0051]: https://github.com/wavedb/wavedb/blob/main/rfcs/0051-ordered-record-lists.md
    #[must_use]
    fn list_key(&self, index: usize) -> Vec<u8> {
        let _ = index;
        Vec::new()
    }

    /// The order-preserving ([`IndexKey`](crate::index::IndexKey)-encoded) key
    /// of secondary index `index` for this record's current values. The macro
    /// implements it as a `match` over the declared `#[wavedb::pivot(...)]`
    /// fields; out-of-range indexes yield an empty key (never dispatched —
    /// the engine loops `0..NUM_SECONDARIES`).
    #[must_use]
    fn secondary_key(&self, index: usize) -> Vec<u8> {
        let _ = index;
        Vec::new()
    }

    /// The content-derived anchor key of a `#[wavedb::key(...)]` type —
    /// SeaHash over the declared key fields' wire bytes in declaration
    /// order (the macro implements it via
    /// [`natural_key_hash`](crate::natural_key_hash)). `None` (the
    /// default) means the type is instant-keyed: identity minted at
    /// `insert`, never derived. For a keyed type the identity IS these
    /// field values — `insert` becomes an upsert at the derived anchor and
    /// a save may never address a different one.
    #[must_use]
    fn natural_key(&self) -> Option<u64> {
        None
    }
}
