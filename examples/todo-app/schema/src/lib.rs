//! The todo-app schema crate — compiled into the node AND every client.
//!
//! The M4 target surface: the app's whole wire API is its six `#[server]`
//! functions; **every struct is storage-only** (`store` entries — engine
//! slots, no wire address). The patterns this pins down:
//!
//! - a global username registry at the **system tenant** (0): a Unique
//!   anchor holding the `PivotId` of a NonUnique `UserEntry` collection with
//!   a secondary index on `username`, lazily bootstrapped;
//! - server-side **cross-tenant writes** via `db.as_tenant(..)` (`register`
//!   bootstraps `Auth` + `Profile` + the todo collection in the new tenant's
//!   space) — a seam no client command can reach;
//! - the **profile→pivot path**: every todo function re-derives the
//!   collection from `Profile::get(db)`; a `PivotId` never crosses the wire.
//!
//! Auth here is a placeholder (sha256 + timestamp token) — real tokens and
//! the permission gates are M8.

// The DbHandle-generic helpers hold `&D` across awaits: their futures are
// only `Send` when the context is — the workspace stance on every
// Store-generic seam.
#![allow(clippy::future_not_send)]

use wavedb::prelude::*;

// ── Exposure: what each side actually serves / can call ───────────────────
//
// The lists ARE the registry. Only the functions are wire-reachable; the
// `store` entries register each type's engine slots so the node can open its
// `PageStore` — a client command naming any struct hash is refused as
// unknown, indistinguishable from a type that never existed.

wavedb::expose_server! {
    fn register, fn login, fn refresh, fn logout,
    fn add_todo, fn all_todos, fn complete_todo, fn delete_todo,
    store AllUserNamesToTenants,
    store UserEntry,
    store Auth,
    store Profile,
    store Todo,
    store wavedb::auth::AuthSession,
    store wavedb::auth::AuthSessions,
}

wavedb::expose_client! {
    fn register, fn login, fn refresh, fn logout,
    fn add_todo, fn all_todos, fn complete_todo, fn delete_todo,
}

// ── Global username registry (system tenant = 0) ──────────────────────────

/// Unique registry record that lives at the system tenant (0). Holds the
/// `PivotId` of the entire username→tenant collection.
#[wavedb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllUserNamesToTenants {
    pub entries: <UserEntry as WaveDbStruct>::PivotId,
}

/// One record per registered user. The secondary index on `username` gives
/// the O(log n) lookup `register`/`login` need.
#[wavedb(NonUnique)]
#[wavedb::pivot(username)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserEntry {
    pub username: String,
    pub tenant_id: u64,
}

// ── Per-tenant records ─────────────────────────────────────────────────────

/// Auth — Unique, one per tenant. Placeholder hash (real Argon2 later);
/// sessions live in `wavedb::auth` records now.
#[wavedb]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Auth {
    pub password_hash: String,
}

/// Profile — Unique, one per tenant. Owns the todo collection handle.
#[wavedb]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub username: String,
    pub todos: <Todo as WaveDbStruct>::PivotId,
}

/// Todo item — NonUnique, many per tenant, ordered by insertion time.
#[wavedb(NonUnique)]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Todo {
    pub title: String,
    pub completed: bool,
}

// ── Auth server functions (called on a system-tenant connection) ──────────

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

// ── Todo server functions (called on the user's tenant connection) ────────

/// Add a new todo. Returns the stable record `Id`.
#[server]
pub async fn add_todo(db: &Db, title: String) -> Result<Id> {
    let profile = get_profile(db).await?;
    Todo::collection(profile.todos)
        .insert(
            db,
            &Todo {
                title,
                completed: false,
            },
        )
        .await
}

/// Every todo in insertion order (`CREATED_AT` ascending) — an async
/// iterator streamed item-by-item over the wire (there is no query DSL;
/// filtered/derived reads are functions like this).
#[server]
pub fn all_todos(db: &Db) -> impl Stream<Item = Result<Todo>> {
    async_profile_todos(db)
}

/// The stream behind [`all_todos`]: resolve the profile, then walk its
/// collection — one `try_stream`-free composition over the handle.
fn async_profile_todos<D: DbHandle<Error = Error>>(
    db: &D,
) -> impl Stream<Item = Result<Todo>> {
    futures::stream::once(get_profile(db))
        .map(move |profile| match profile {
            Ok(p) => Todo::collection(p.todos).all(db).left_stream(),
            Err(e) => {
                futures::stream::once(std::future::ready(Err(e))).right_stream()
            }
        })
        .flatten()
}

/// Mark a todo completed (the old version stays on the history chain).
#[server]
pub async fn complete_todo(db: &Db, id: Id) -> Result<()> {
    let profile = get_profile(db).await?;
    let col = Todo::collection(profile.todos);
    let mut todo = col
        .get(db, id)
        .await?
        .ok_or_else(|| Error::not_found("todo not found"))?;
    todo.completed = true;
    col.save(db, id, &todo).await
}

/// Remove a todo (moved to the dead tree — bytes kept, history navigable).
#[server]
pub async fn delete_todo(db: &Db, id: Id) -> Result<()> {
    let profile = get_profile(db).await?;
    Todo::collection(profile.todos).remove(db, id).await?;
    Ok(())
}

// ── Private helpers — generic over the execution context ──────────────────

/// Lazily initialise the global username registry on first call. Generic
/// over [`DbHandle`], so the same helper serves the node bodies and any
/// engine-local test.
async fn ensure_registry<D: DbHandle>(
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
async fn get_profile<D: DbHandle<Error = Error>>(db: &D) -> Result<Profile> {
    Profile::get(db)
        .await?
        .ok_or_else(|| Error::not_found("profile missing"))
}

fn hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::new().chain_update(password).finalize())
}

/// Mint a 48-bit tenant id from the current nanosecond timestamp — a
/// placeholder allocator (collisions astronomically unlikely at demo scale).
fn new_tenant_id() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let masked = nanos & u128::from(U48::MASK);
    // Masked to 48 bits, so the narrowing is infallible.
    u64::try_from(masked).expect("48-bit value fits u64")
}
