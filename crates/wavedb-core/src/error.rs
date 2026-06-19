//! The workspace error type.

use thiserror::Error;

/// Errors raised by `wavedb-core` — today these are all wire (de)serialization
/// faults; more variants join as the engine grows.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// A reader ran past the end of its buffer.
    #[error("unexpected end of wire buffer")]
    UnexpectedEof,
    /// A `String` field held bytes that were not valid UTF-8.
    #[error("invalid utf-8 in wire string")]
    Utf8,
    /// A `char` field held a `u32` that is not a Unicode scalar value.
    #[error("invalid char scalar {0:#x}")]
    InvalidChar(u32),
    /// An enum field held a tag outside the declared variant range.
    #[error("invalid enum tag {0}")]
    InvalidTag(u8),
    /// A `bool` field held a byte other than `0` or `1`.
    #[error("invalid bool byte {0}")]
    InvalidBool(u8),
    /// A value handed to `U48::new` did not fit in 48 bits.
    #[error("value {0} exceeds 48 bits")]
    U48Overflow(u64),
}

/// Shorthand for a `Result` carrying the workspace [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
