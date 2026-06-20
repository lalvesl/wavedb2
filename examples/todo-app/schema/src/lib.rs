use futures::StreamExt;
use wavedb::prelude::*;

// Pull in the generated REGISTRY (Object enum + per-struct descriptors).
include!(concat!(env!("OUT_DIR"), "/wavedb_registry.rs"));

// ── Data model ────────────────────────────────────────────────────────────────

/// Auth — Unique, one per tenant.
/// Stores the password hash and the current session token.
#[wavedb]
pub struct Auth {
    pub password_hash: String,
    pub session_token: Option<String>,
}

/// Profile — Unique, one per tenant.
/// Holds the username and the PivotId of the user's Todo collection.
#[wavedb]
pub struct Profile {
    pub username: String,
    pub todos: <Todo as WaveDbStruct>::PivotId,
}

/// Todo — NonUnique, many per tenant, ordered by insertion time.
#[wavedb(NonUnique)]
pub struct Todo {
    pub title: String,
    pub completed: bool,
}

// ── Auth server functions ─────────────────────────────────────────────────────

/// Create a new user for the calling tenant.
/// Errors if the tenant already has an Auth record.
#[server]
pub async fn register(db: &Db, username: String, password: String) -> Result<()> {
    if Auth::get(db).await?.is_some() {
        return Err(Error::already_exists("user already registered"));
    }
    Auth {
        password_hash: hash_password(&password),
        session_token: None,
    }
    .save(db)
    .await?;

    let todos = Todo::create_pivot(db).await?;
    Profile { username, todos }.save(db).await
}

/// Verify the password and return a fresh session token.
#[server]
pub async fn login(db: &Db, password: String) -> Result<String> {
    let auth = Auth::get(db).await?.ok_or(Error::not_found("user not registered"))?;
    if auth.password_hash != hash_password(&password) {
        return Err(Error::unauthorized("wrong password"));
    }
    let token = new_token();
    Auth {
        session_token: Some(token.clone()),
        ..auth
    }
    .save(db)
    .await?;
    Ok(token)
}

// ── Todo server functions ─────────────────────────────────────────────────────

/// Add a new todo item. Returns the stable record Id.
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

/// Mark a todo as completed (updates in place; old version kept in history).
#[server]
pub async fn complete_todo(db: &Db, id: Id) -> Result<()> {
    let profile = get_profile(db).await?;
    let col = Todo::collection(db, profile.todos);
    let mut todo = col.get(db, id).await?.ok_or(Error::not_found("todo not found"))?;
    todo.completed = true;
    todo.save(db).await
}

/// Remove a todo (moves to dead BpTree — history kept, record not erased).
#[server]
pub async fn delete_todo(db: &Db, id: Id) -> Result<()> {
    let profile = get_profile(db).await?;
    Todo::collection(db, profile.todos).remove(db, id).await
}

// ── Private helpers (server-side only, not callable from client) ──────────────

async fn get_profile(db: &Db) -> Result<Profile> {
    Profile::get(db)
        .await?
        .ok_or(Error::not_found("profile missing — call register first"))
}

fn hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(password.as_bytes());
    format!("{:x}", h.finalize())
}

fn new_token() -> String {
    use sha2::{Digest, Sha256};
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut h = Sha256::new();
    h.update(ts.to_le_bytes());
    format!("{:x}", h.finalize())
}
