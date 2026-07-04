//! `wavedb-core` — primitives shared by every node kind and by proc-macro
//! generated code: the composite [`Id`], the [`U48`] newtype, the [`WaveWire`]
//! serialization format, and the workspace [`Error`]. **No I/O.**
//!
//! See `crates/wavedb-core/README.md` for the design.

// Pedantic/nursery lints that fight terse, byte-precise (de)serialization code.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::missing_const_for_fn
)]
// The `Store` / `BpTree` backend contracts use `async fn` in traits deliberately:
// these are internal seams, not a public Send-bounded API, so the absence of an
// auto `Send` bound is intended, not an oversight. Same stance for the futures
// `BpTree`'s methods return — they are only `Send` when the backing `Store` is.
#![allow(async_fn_in_trait)]
#![allow(clippy::future_not_send)]

// The derive (once it lands in `wavedb-macros`) emits absolute
// `::wavedb_core::` paths; this lets the crate use its own derive.
extern crate self as wavedb_core;

pub mod collection;
mod collection_write;
pub mod error;
pub mod expose;
pub mod hooks;
pub mod id;
pub mod index;
pub mod local_id;
pub mod metadata;
pub mod overlay;
pub mod permission;
pub mod record;
pub mod store;
pub mod traits;
pub mod u48;
pub mod wire;

pub use collection::Collection;
pub use error::{Error, Result};
// Generated `by_<field>` lookups return `impl Stream`; re-exported so macro
// expansions can name it without adding a `futures` dep to schema crates.
pub use futures::Stream;
pub use hooks::LookupHooks;
pub use id::Id;
pub use index::{
    Bound, BpTree, Except, IdStreamExt, IndexKey, Intersect, NodeKey, Pivot,
    SecKey, Union,
};
pub use local_id::LocalId;
pub use metadata::Metadata;
pub use permission::PermissionRef;
pub use store::{Store, Write};
pub use traits::{
    NonUniqueStruct, PivotHandle, Shape, UniqueStruct, WaveDbStruct,
};
pub use u48::U48;
pub use wire::{Cursor, WaveWire, from_wire, to_wire};
