//! Minimal client↔server example: a Unique record (`ContactBook`) owning the
//! `PivotId` of a NonUnique collection (`Contact`), driven from a remote
//! client over the wire — insert / update (`save`) / remove, plus the
//! secondary index on `city`.
//!
//! Bootstrap is server-side: `create_pivot` has no wire command, so the one
//! `#[server]` function here (`open_book`) lazily creates the pivot and saves
//! it inside the Unique holder. After that the client reads the holder with
//! `ContactBook::get(&db)` and drives the collection directly — `Contact` is
//! listed in both exposures, so its collection commands are wire-reachable.

// The typed handle futures hold `&Db` across awaits — non-Send by design
// (current-thread model), the workspace stance.
#![allow(clippy::future_not_send)]

use wavedb::prelude::*;

// The lists ARE the registry: `open_book` and the two structs' generated
// commands are wire-reachable; anything else refuses as an unknown hash.
wavedb::expose_server! {
    fn open_book,
    fn contacts_in,
    ContactBook,
    Contact,
}

wavedb::expose_client! {
    fn open_book,
    fn contacts_in,
    ContactBook,
    Contact,
}

/// Unique — one per tenant. The owning record: holds the collection handle.
#[wavedb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactBook {
    pub owner: String,
    pub contacts: <Contact as WaveDbStruct>::PivotId,
}

/// NonUnique — many per tenant, secondary index on `city` (`by_city`).
#[wavedb(NonUnique)]
#[wavedb::pivot(city)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Contact {
    pub name: String,
    pub phone: String,
    pub city: String,
}

/// Ensure this tenant's book exists (idempotent): first call creates the
/// `Contact` pivot and saves the `ContactBook` holding it.
#[server]
pub async fn open_book(db: &Db, owner: String) -> Result<()> {
    if ContactBook::get(db).await?.is_some() {
        return Ok(());
    }
    let contacts = Contact::create_pivot(db).await?;
    ContactBook { owner, contacts }.save(db).await?;
    Ok(())
}

/// Every contact living in `city`, via the `by_city` secondary index —
/// filtered reads are `#[server]` functions (there is no query DSL, and
/// `search_by` has no wire command yet).
#[server]
pub async fn contacts_in(db: &Db, city: String) -> Result<Vec<Contact>> {
    use futures::TryStreamExt as _;
    let book = ContactBook::get(db)
        .await?
        .ok_or_else(|| Error::not_found("book missing — call open_book"))?;
    Contact::collection(book.contacts)
        .by_city(db, &city)
        .try_collect()
        .await
}
