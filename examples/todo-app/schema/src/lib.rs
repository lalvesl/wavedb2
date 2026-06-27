use futures::StreamExt;
use wavedb::prelude::*;

include!(concat!(env!("OUT_DIR"), "/wavedb_registry.rs"));

// ── Global username registry (system tenant = 0) ──────────────────────────────

/// Unique registry record that lives at system tenant (0).
/// Holds the PivotId of the entire username→tenant collection.
#[wavedb]
pub struct AllUserNamesToTenants {
    pub entries: <UserEntry as WaveDbStruct>::PivotId,
}

/// One record per registered user.
/// Secondary index on `username` allows O(log n) lookup by name.
#[wavedb(NonUnique)]
#[wavedb::pivot(username)]
pub struct UserEntry {
    pub username: String,
    pub tenant_id: u64,
}

// ── Per-tenant records ────────────────────────────────────────────────────────

/// Auth — Unique, one per tenant.
#[wavedb]
pub struct Auth {
    pub password_hash: String,
    pub session_token: Option<String>,
}

/// Profile — Unique, one per tenant. Owns the todo collection handle.
#[wavedb]
pub struct Profile {
    pub username: String,
    pub todos: <Todo as WaveDbStruct>::PivotId,
}

/// Todo item — NonUnique, many per tenant, ordered by insertion time.
#[wavedb(NonUnique)]
pub struct Todo {
    pub title: String,
    pub completed: bool,
}

// ── Auth server functions (call with system tenant db, tenant = 0) ────────────

/// Register a new user. Allocates a tenant id, writes the global UserEntry,
/// and bootstraps Auth + Profile in the new tenant's space.
/// Returns the assigned tenant_id — the client stores it and reconnects.
#[server(public)]
pub async fn register(db: &Db, username: String, password: String) -> Result<u64> {
    let registry = ensure_registry(db).await?;
    let col = UserEntry::collection(db, registry.entries);

    if col.by_username(db, &username).next().await.is_some() {
        return Err(Error::already_exists("username already taken"));
    }

    let tenant_id = new_tenant_id();
    col.insert(db, UserEntry { username: username.clone(), tenant_id }).await?;

    // bootstrap the new tenant's own records (server-side cross-tenant write)
    let user_db = db.as_tenant(tenant_id);
    Auth { password_hash: hash_password(&password), session_token: None }
        .save(&user_db)
        .await?;
    let todos = Todo::create_pivot(&user_db).await?;
    Profile { username, todos }.save(&user_db).await?;

    Ok(tenant_id)
}

/// Verify credentials. Returns (tenant_id, session_token).
/// Client uses the returned tenant_id to open its own tenant connection.
#[server(public)]
pub async fn login(db: &Db, username: String, password: String) -> Result<(u64, String)> {
    let registry = ensure_registry(db).await?;
    let col = UserEntry::collection(db, registry.entries);

    let entry = col
        .by_username(db, &username)
        .next()
        .await
        .ok_or(Error::not_found("user not found"))??;

    let user_db = db.as_tenant(entry.tenant_id);
    let auth = Auth::get(&user_db).await?.ok_or(Error::not_found("auth record missing"))?;
    if auth.password_hash != hash_password(&password) {
        return Err(Error::unauthorized("wrong password"));
    }

    let token = new_token();
    Auth { session_token: Some(token.clone()), ..auth }.save(&user_db).await?;

    Ok((entry.tenant_id, token))
}

// ── Todo server functions (call with user tenant db) ──────────────────────────

/// Add a new todo. Returns the stable record Id.
#[server]
pub async fn add_todo(db: &Db, title: String) -> Result<Id> {
    let profile = get_profile(db).await?;
    Todo::collection(db, profile.todos)
        .insert(db, Todo { title, completed: false })
        .await
}

/// Stream all todos in insertion order (CREATED_AT ascending).
#[server]
pub fn all_todos(db: &Db) -> impl Stream<Item = Result<Todo>> {
    async_stream::try_stream! {
        let profile = get_profile(db).await?;
        let mut s = Todo::collection(db, profile.todos).all(db);
        while let Some(item) = s.next().await {
            yield item?;
        }
    }
}

/// Mark a todo completed (old version kept in history chain).
#[server]
pub async fn complete_todo(db: &Db, id: Id) -> Result<()> {
    let profile = get_profile(db).await?;
    let col = Todo::collection(db, profile.todos);
    let mut todo = col.get(db, id).await?.ok_or(Error::not_found("todo not found"))?;
    todo.completed = true;
    todo.save(db).await
}

/// Remove a todo (moves to dead BpTree — record bytes kept, history navigable).
#[server]
pub async fn delete_todo(db: &Db, id: Id) -> Result<()> {
    let profile = get_profile(db).await?;
    Todo::collection(db, profile.todos).remove(db, id).await
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Lazily initialise the global registry on first call.
async fn ensure_registry(db: &Db) -> Result<AllUserNamesToTenants> {
    if let Some(r) = AllUserNamesToTenants::get(db).await? {
        return Ok(r);
    }
    let entries = UserEntry::create_pivot(db).await?;
    let r = AllUserNamesToTenants { entries };
    r.save(db).await?;
    Ok(r)
}

async fn get_profile(db: &Db) -> Result<Profile> {
    Profile::get(db)
        .await?
        .ok_or(Error::not_found("profile missing"))
}

fn hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::new().chain_update(password).finalize())
}

fn new_token() -> String {
    use sha2::{Digest, Sha256};
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}", Sha256::new().chain_update(ts.to_le_bytes()).finalize())
}

/// Mint a 48-bit tenant id from the current nanosecond timestamp.
fn new_tenant_id() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        & 0x0000_FFFF_FFFF_FFFF // mask to 48 bits (TENANT field width)
}
