//! `wavedb-core` — primitives shared by every node kind and by proc-macro
//! generated code: the composite [`Id`], the [`U48`] newtype, the [`Wire`]
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

// The derive (once it lands in `wavedb-macros`) emits absolute
// `::wavedb_core::` paths; this lets the crate use its own derive.
extern crate self as wavedb_core;

pub mod error;
pub mod id;
pub mod u48;
pub mod wire;

pub use error::{Error, Result};
pub use id::Id;
pub use u48::U48;
pub use wire::{from_wire, to_wire, Cursor, Wire};
