//! `wavedb-storage` — the per-node engine: block manager, per-`STRUCT_HASH`
//! page directory (linear hashing), page format, dictionaries, journal pipeline.
//!
//! Built bottom-up. The [`block`] layer (descriptor + allocator) is in place; the
//! directory, page format, dictionaries, and pipeline follow. See
//! `crates/wavedb-storage/README.md` for the target design.

// Byte-precise packing/hashing code casts deliberately between integer widths.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss
)]

pub mod block;
pub mod directory;

pub use block::{BLOCK_SIZE, BlockAllocator, BlockDescriptor, Run};
pub use directory::{Directory, bucket_index, hash_of};
