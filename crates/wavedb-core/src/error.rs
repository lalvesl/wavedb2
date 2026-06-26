//! The workspace error type.

use thiserror::Error;

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
}

/// Shorthand for a `Result` carrying the workspace [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
