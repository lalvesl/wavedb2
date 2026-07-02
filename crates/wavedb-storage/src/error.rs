//! The storage-engine error type.
//!
//! Distinct from [`wavedb_core::Error`]: this layer does real file I/O, so it adds
//! I/O and on-disk-corruption variants. Wire/codec faults from core flow through
//! [`StorageError::Core`].

use thiserror::Error;

/// Errors raised by `wavedb-storage`.
#[derive(Debug, Error)]
pub enum StorageError {
    /// An underlying filesystem error.
    #[error("storage io: {0}")]
    Io(#[from] std::io::Error),
    /// `data.bin`'s superblock did not start with the expected magic — not a
    /// WaveDB data file (or a corrupt one).
    #[error("not a wavedb data file (bad magic)")]
    BadMagic,
    /// `data.bin` was written by an incompatible on-disk format version.
    #[error("unsupported data.bin format version {0}")]
    BadVersion(u32),
    /// A read/write addressed blocks past the current end of the file.
    #[error("data.bin too short: need {need} bytes, file has {have}")]
    OutOfBounds {
        /// Bytes the operation required.
        need: u64,
        /// Bytes the file actually holds.
        have: u64,
    },
    /// A buffer handed to a positioned write exceeded its target run.
    #[error("write of {got} bytes exceeds run capacity {cap}")]
    RunOverflow {
        /// Bytes the caller tried to write.
        got: u64,
        /// Byte capacity of the target run.
        cap: u64,
    },
    /// An on-disk structure failed an integrity check on read (crc, bounds, tag).
    #[error("corrupt {0}")]
    Corrupt(&'static str),
    /// A core/codec fault surfaced inside the engine.
    #[error(transparent)]
    Core(#[from] wavedb_core::Error),
}

/// Shorthand for a `Result` carrying [`StorageError`].
pub type StorageResult<T> = core::result::Result<T, StorageError>;

/// Flatten an engine error into [`wavedb_core::Error::Backend`] at the [`Store`]
/// boundary — core declares the trait but stays I/O-free, so it can't name a
/// concrete disk fault.
///
/// [`Store`]: wavedb_core::Store
impl From<StorageError> for wavedb_core::Error {
    fn from(e: StorageError) -> Self {
        Self::Backend(e.to_string())
    }
}
