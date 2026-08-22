//! Private helpers the `#[server]` bodies lean on — split from
//! [`lib`](crate) for the file budget.
//!
//! Every item here is **server-side only**. They exist to serve `#[server]`
//! bodies, so they carry `#[cfg(feature = "server-side")]` by hand and are
//! cfg-gated out of a client artifact along with the bodies themselves: that
//! is the schema author's half of the no-leak contract, and the macros cannot
//! do it for code that sits outside a `#[server]` fn.

use wavedb::prelude::*;

use crate::{AllUserNamesToTenants, Profile, UserEntry};

/// engine-local test.
#[cfg(feature = "server-side")]
pub async fn ensure_registry<D: DbHandle>(
    db: &D,
) -> core::result::Result<AllUserNamesToTenants, D::Error> {
    if let Some(r) = AllUserNamesToTenants::get(db).await? {
        return Ok(r);
    }
    let entries = UserEntry::create_pivot(db).await?;
    let r = AllUserNamesToTenants { entries };
    r.save(db).await?;
    Ok(r)
}

/// The caller tenant's profile — the root of the profile→pivot path.
#[cfg(feature = "server-side")]
pub async fn get_profile<D: DbHandle<Error = Error>>(
    db: &D,
) -> Result<Profile> {
    Profile::get(db)
        .await?
        .ok_or_else(|| Error::not_found("profile missing"))
}

#[cfg(feature = "server-side")]
pub fn hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::new().chain_update(password).finalize())
}

/// Mint a 48-bit tenant id from the current nanosecond timestamp — a
/// placeholder allocator (collisions astronomically unlikely at demo scale).
#[cfg(feature = "server-side")]
pub fn new_tenant_id() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let masked = nanos & u128::from(U48::MASK);
    // Masked to 48 bits, so the narrowing is infallible.
    u64::try_from(masked).expect("48-bit value fits u64")
}
