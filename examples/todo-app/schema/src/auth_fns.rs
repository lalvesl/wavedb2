//! The auth half of the wire API: the four `#[server]` functions a client
//! calls **on a system-tenant connection**, before it knows its own tenant.
//!
//! Split from [`lib`](crate) for the file budget, along the seam the app
//! already has — everything here runs at tenant 0 against the global username
//! registry, and everything left behind runs inside one user's space.
//!
//! `register` is where the cross-tenant seam lives: `db.as_tenant(..)`
//! bootstraps the new user's records from the system connection, which is a
//! move no client command can make.

use wavedb::prelude::*;

use crate::helpers::{ensure_registry, hash_password, new_tenant_id};
use crate::{Auth, Profile, Todo, UserEntry, UserEntrySecondaries};

/// Register a new user: allocate a tenant id, write the global `UserEntry`,
/// and bootstrap `Auth` + `Profile` (+ the todo collection) in the new
/// tenant's space. Returns the assigned tenant id — the client stores it and
/// reconnects as that tenant.
#[server(public)]
pub async fn register(
    db: &Db,
    username: String,
    password: String,
) -> Result<u64> {
    let registry = ensure_registry(db).await?;
    let col = UserEntry::collection(registry.entries);

    // Scope the lookup stream so its borrow of `username` ends before the
    // insert consumes it.
    {
        let mut existing = std::pin::pin!(col.by_username(db, &username));
        if existing.next().await.is_some() {
            return Err(Error::already_exists("username already taken"));
        }
    }

    let tenant_id = new_tenant_id();
    col.insert(
        db,
        &UserEntry {
            username: username.clone(),
            tenant_id,
        },
    )
    .await?;

    // Bootstrap the new tenant's own records — the server-side cross-tenant
    // seam (`as_tenant` never crosses the wire).
    let user_db = db.as_tenant(U48::try_from(tenant_id)?);
    Auth {
        password_hash: hash_password(&password),
    }
    .save(&user_db)
    .await?;
    let todos = Todo::create_pivot(&user_db).await?;
    Profile { username, todos }.save(&user_db).await?;

    Ok(tenant_id)
}

/// Verify credentials and open a session. Returns
/// `(tenant_id, token pair)`: the client reconnects with the access token
/// and keeps the refresh token to mint the next pair.
#[server(public)]
pub async fn login(
    db: &Db,
    username: String,
    password: String,
) -> Result<(u64, wavedb::TokenPair)> {
    let registry = ensure_registry(db).await?;
    let col = UserEntry::collection(registry.entries);

    let mut matches = std::pin::pin!(col.by_username(db, &username));
    let entry = matches
        .next()
        .await
        .ok_or_else(|| Error::not_found("user not found"))??;

    let tenant = U48::try_from(entry.tenant_id)?;
    let user_db = db.as_tenant(tenant);
    let auth = Auth::get(&user_db)
        .await?
        .ok_or_else(|| Error::not_found("auth record missing"))?;
    if auth.password_hash != hash_password(&password) {
        return Err(Error::unauthorized("wrong password"));
    }

    let pair = wavedb::auth::issue_pair(&user_db, tenant).await?;
    Ok((entry.tenant_id, pair))
}

/// Trade a refresh token for the next pair (rotates it; a replayed token
/// revokes the whole session). Public: the caller's access token may
/// already be dead — the refresh token itself is the credential.
#[server(public)]
pub async fn refresh(
    db: &Db,
    tenant_id: u64,
    token: Vec<u8>,
) -> Result<wavedb::TokenPair> {
    let user_db = db.as_tenant(U48::try_from(tenant_id)?);
    wavedb::auth::refresh_pair(&user_db, &token).await
}

/// Revoke the session behind `token` (logout): its next refresh fails and
/// the outstanding access token dies within one TTL.
#[server(public)]
pub async fn logout(db: &Db, tenant_id: u64, token: Vec<u8>) -> Result<()> {
    let user_db = db.as_tenant(U48::try_from(tenant_id)?);
    wavedb::auth::revoke(&user_db, &token).await
}
