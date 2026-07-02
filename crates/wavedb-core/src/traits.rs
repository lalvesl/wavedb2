//! `WaveDbStruct` — the per-struct contract the `#[wavedb]` proc-macro implements,
//! and the [`Shape`] marker that records a type's cardinality discipline.
//!
//! Core only declares the surface; the macro fills in `STRUCT_HASH`, `SHAPE`, and
//! the generated `PivotId` for each declared object.

use crate::wire::WaveWire;

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
}
