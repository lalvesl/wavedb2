//! The typed `Unique` client surface, as methods on [`Db`].
//!
//! `db.get::<T>()` / `db.save(&value)`, gated to [`UniqueStruct`] (the macro's
//! default shape) so a `NonUnique` type — reached through its collection — can
//! never be driven here at compile time.
//!
//! (The methods live on `Db` rather than as `T::get(&db)` because the macro
//! already emits the `Store`-generic `T::get(store, tenant)` inherent methods;
//! unifying the spelling is a later macro re-plumb.)

use wavedb_core::UniqueStruct;
use wavedb_core::expose::Command;
use wavedb_core::wire::to_wire;

use crate::db::Db;
use crate::error::Result;
use crate::reply;

impl Db {
    /// Fetch this tenant's `Unique` record of type `T`. `None` = never saved.
    ///
    /// # Errors
    /// A failed call, or a decode fault.
    pub async fn get<T: UniqueStruct>(&self) -> Result<Option<T>> {
        let r = self
            .command(T::STRUCT_HASH, Command::Get, Vec::new())
            .await?;
        reply::value(r)
    }

    /// Save (insert-or-overwrite) this tenant's `Unique` record. A save over
    /// an existing record archives the superseded version node-side (the
    /// timeline stays walkable).
    ///
    /// # Errors
    /// A failed call, or an unexpected reply.
    pub async fn save<T: UniqueStruct>(&self, value: &T) -> Result<()> {
        let r = self
            .command(T::STRUCT_HASH, Command::Save, to_wire(value))
            .await?;
        reply::done(&r)
    }

    /// This tenant's `Unique` record versions, **newest-first** (the live
    /// record, then each archived version along the modification chain).
    /// Empty when never saved. Buffered for now — a streaming iterator lands
    /// with the transport's stream frames.
    ///
    /// # Errors
    /// A failed call, or a decode fault on any version.
    pub async fn history<T: UniqueStruct>(&self) -> Result<Vec<T>> {
        let r = self
            .command(T::STRUCT_HASH, Command::History, Vec::new())
            .await?;
        reply::values(r)
    }
}
