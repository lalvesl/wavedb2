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

    /// The typed handle into this type's collection.
    ///
    /// For a `NonUnique` type the macro generates a concrete `PivotId` (a wrapper
    /// over a [`LocalId`](crate::local_id::LocalId)) that callers store in a field
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
}
