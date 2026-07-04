//! `wavedb` — the user-facing client library.
//!
//! The one dependency an application's schema crate and clients need: it
//! re-exports the `#[wavedb]` / `#[server]` derives and the `expose_*!`
//! declaration macros, the core value types, and the [`Db`] handle with its
//! typed CRUD.
//!
//! ```text
//! use wavedb::prelude::*;
//!
//! let db = Db::connect("127.0.0.1:7700", 42.into(), 42.into()).await?;
//!
//! // Unique: one record per tenant; save is an upsert.
//! let profile: Option<AboutUser> = db.get::<AboutUser>().await?;
//! db.save(&AboutUser { city: "Lisbon".into() }).await?;
//!
//! // NonUnique: open the collection from a stored handle, then mutate.
//! let orders = db.collection::<Order>(user.orders);
//! let id = orders.insert(Order { amount: 120 }).await?;
//! let one = orders.get(id).await?;
//! ```
//!
//! **M4 scope.** The transport is HTTP POST (`wavedb-net`); there is no local
//! write-through cache yet (`Db::open`, M6) and no `#[server]` call stubs yet
//! (the derive is a separate step). Collection reads are point `get` by `Id`;
//! streaming walks land with the transport's stream frames. The typed surface
//! lives on [`Db`] (`db.get::<T>()`) because the macro already emits the
//! `Store`-generic `T::get(store, tenant)` inherent methods — unifying the two
//! spellings is a later macro re-plumb.

// Typed calls hold `&Db` across awaits: their futures are only `Send` when the
// transport is, which the current-thread client path never requires. The typed
// object traits use `async fn` deliberately — a public but internal-shaped
// seam, not a `Send`-bounded API. Same stance the core engine seams take.
#![allow(clippy::future_not_send, async_fn_in_trait)]

mod collection;
mod db;
mod error;
mod reply;
mod server_db;
mod unique;

pub use collection::ClientCollection;
pub use db::Db;
pub use error::{Error, Result};
pub use server_db::{ServerCollection, ServerDb};

// The compile-time front door, re-exported so a schema crate names one
// dependency: `wavedb::wavedb` / `wavedb::server` / `wavedb::expose_*!`.
pub use wavedb_macros::{expose_client, expose_server, server, wavedb};

/// Everything an application touches, in one glob.
pub mod prelude {
    pub use crate::{ClientCollection, Db, Error, Result, ServerDb};

    // The declaration + object macros (one import surface for schema crates).
    pub use wavedb_macros::{expose_client, expose_server, server, wavedb};

    // Core value types and the traits generated code and app code name.
    pub use wavedb_core::{
        Id, LocalId, Metadata, NonUniqueStruct, PermissionRef, PivotHandle,
        U48, UniqueStruct, WaveDbStruct, WaveWire,
    };

    // Collection iterators are async streams.
    pub use futures::{Stream, StreamExt, TryStreamExt};
}
