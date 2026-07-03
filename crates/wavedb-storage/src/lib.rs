//! `wavedb-storage` — the per-node engine: block manager, per-`STRUCT_HASH`
//! page directory (linear hashing), page format, dictionaries, journal pipeline.
//!
//! Built bottom-up. The [`block`] layer (descriptor + allocator), the
//! [`directory`] addressing math, and the [`block_file`] I/O seam are in place;
//! the page format, dictionaries, and journal pipeline follow. See
//! `crates/wavedb-storage/README.md` for the target design.

// Byte-precise packing/hashing code casts deliberately between integer widths.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss
)]
// `PageStore`'s `Store` impl returns futures that are only `Send` when the
// caller's usage is. These are internal node-side seams, not a public
// `Send`-bounded API, so the missing auto-`Send` is intended — same stance core
// takes with `async_fn_in_trait`.
#![allow(clippy::future_not_send)]

pub mod block;
pub mod block_file;
pub mod dictionary;
pub mod directory;
mod directory_pages;
pub mod error;
pub mod journal;
pub mod page;
pub mod page_store;

pub use block::{BLOCK_SIZE, BlockAllocator, BlockDescriptor, Run};
pub use block_file::{BlockFile, RESERVED_BLOCKS};
pub use dictionary::Dictionary;
pub use directory::{Directory, bucket_index, hash_of};
pub use error::{StorageError, StorageResult};
pub use journal::Journal;
pub use page::SlotPage;
pub use page_store::PageStore;
