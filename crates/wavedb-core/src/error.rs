//! The workspace error type.

use thiserror::Error;

use crate::id::Id;
use crate::local_id::LocalId;

/// Errors raised by `wavedb-core`. Wire (de)serialization faults arrive through
/// the [`Wire`](Error::Wire) variant (from the standalone `wavedb-wire` crate);
/// the rest are core/engine concerns.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// A wire (de)serialization fault — a buffer/size mismatch or an intrinsic
    /// per-type check (see [`wavedb_wire::Error`]).
    #[error(transparent)]
    Wire(#[from] wavedb_wire::Error),
    /// A value handed to `U48::new` did not fit in 48 bits.
    #[error("value {0} exceeds 48 bits")]
    U48Overflow(u64),
    /// A wire envelope carried a `STRUCT_HASH` not declared in this build's
    /// registry (a record written under a schema this binary doesn't know).
    #[error("unknown struct hash {0:#018x}")]
    UnknownStructHash(u64),
    /// A `BpTree` node pointer resolved to nothing in the backing
    /// [`Store`](crate::Store) — a dangling root/child pointer (index out of
    /// sync with the store).
    #[error("bptree node {0:?} missing")]
    BpTreeNodeMissing(LocalId),
    /// A value read as a `BpTree` node did not start with the reserved node
    /// tag — the pointer resolved to some other kind of value.
    #[error("bptree node bad page-kind tag {0:#018x}")]
    BpTreeNodeBadTag(u64),
    /// A collection handle's `Pivot` record was not in the [`Store`] — a stale
    /// or foreign `PivotId`.
    ///
    /// [`Store`]: crate::Store
    #[error("pivot record {0:?} missing")]
    PivotMissing(LocalId),
    /// An index pointed at a record the [`Store`] no longer holds — index out
    /// of sync with the record space.
    ///
    /// [`Store`]: crate::Store
    #[error("record {0:?} missing")]
    RecordMissing(Id),
    /// A secondary-index lookup named an index this collection's `Pivot` does
    /// not declare (out of `0..NUM_SECONDARIES`).
    #[error("secondary index {0} out of range")]
    SecondaryIndexOutOfRange(usize),
    /// The caller's identity tier may not perform this operation (M8 gate:
    /// an unauthenticated caller on a login-required item). The message is
    /// evidence for the log; the wire flattens it to one uniform kind.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    /// A failure inside a [`Store`](crate::Store) backend — disk I/O, on-disk
    /// corruption, or similar. Core stays I/O-free, so the concrete cause is
    /// flattened to a message at the trait boundary.
    #[error("storage backend: {0}")]
    Backend(String),
}

/// Shorthand for a `Result` carrying the workspace [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
