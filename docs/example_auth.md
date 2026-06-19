# Worked example — auth + a typed request

An end-to-end sketch of how authentication and a normal request fit together.
**Illustrative**: the API names follow the design (see
[`wavedb-net` §Authentication](../crates/wavedb-net/README.md#authentication) and
[`wavedb-macros` §Server functions](../crates/wavedb-macros/README.md#server-functions--server)),
but nothing here is implemented yet.

The whole flow in one line each:

> **unauth connect → `login` mints access+refresh + a `Session` record →
> `Db::with_token` → `create`/`get`/server-fn → `refresh` rotates → revoke flips
> `Session.revoked`.**

Every read is `get` / `::all` / a `#[server]` function — there is **no query
DSL**. Every server-function body (`login`, `refresh`, `pinned_notes`) compiles
**only into the node**; the client sees typed stubs that ship `Wire`-encoded
arguments over the transport.

```rust
use wavedb::prelude::*;

// ---- schema crate: compiled into client AND node ----------------------------

#[wavedb]                                   // Unique: one per tenant
pub struct AboutUser { pub name: String, pub city: String }

#[wavedb(NonUnique)]                          // many per tenant
pub struct Note { pub body: String, pub pinned: bool }

// Login record — credentials are just data (Unique object per user).
#[wavedb]
pub struct Credentials { pub argon2: String }

// Session record the refresh token is bound to (revocation handle).
#[wavedb]
pub struct Session { pub user: U48, pub tenant: U48, pub issued: u64, pub revoked: bool }

#[derive(Wire)] pub struct Tokens { pub access: String, pub refresh: String }

// ---- server functions: body runs ONLY on the node ---------------------------

#[server]                                    // login: verify credential, mint pair
async fn login(db: &Db, user: U48, password: String) -> Result<Tokens> {
    let cred = Credentials::get_for(db, user).await?.ok_or(Error::NoUser)?;
    argon2_verify(&cred.argon2, &password)?;            // or: verify OAuth token
    let sid = Session { user, tenant: user, issued: now(), revoked: false }
        .create(db).await?;                              // session record = revocation handle
    Ok(Tokens { access: mint_access(user, user), refresh: mint_refresh(sid) })
}

#[server]                                    // revocable refresh
async fn refresh(db: &Db, refresh: String) -> Result<Tokens> {
    let sid = verify_refresh(&refresh)?;                 // HMAC ok...
    let s = Session::get_by_id(db, sid).await?.ok_or(Error::Revoked)?;
    if s.revoked { return Err(Error::Revoked); }         // ...AND record live
    Ok(Tokens { access: mint_access(s.user, s.tenant), refresh: rotate_refresh(sid) })
}

#[server]                                    // filtered read = server fn (no query DSL)
async fn pinned_notes(db: &Db) -> Result<Vec<Note>> {
    Ok(Note::all(db).await?.into_iter().filter(|n| n.pinned).collect())
}

// ---- client (native or wasm): same calls, bodies are stubs over the wire -----

async fn flow() -> Result<()> {
    // 1. connect unauthenticated (user = U48::MAX) — only login + public reads
    let pub_db = Db::connect("wss://app.example", U48::MAX, U48::MAX).await?;

    // 2. login → tokens (access short-TTL stateless, refresh revocable)
    let t = login(&pub_db, U48::from(42), "hunter2".into()).await?;

    // 3. authed handle — node derives user/tenant from the access token
    let db = Db::with_token("wss://app.example", &t.access).await?;

    // 4. ordinary requests: save (versioned) + get (1 lookup)
    Note { body: "ship it".into(), pinned: true }.create(&db).await?;
    let me: Option<AboutUser> = AboutUser::get(&db).await?;

    // 5. filtered read via server fn
    let pins: Vec<Note> = pinned_notes(&db).await?;

    // 6. access expires → swap with refresh (rotates)
    let t = refresh(&pub_db, t.refresh).await?;
    let db = Db::with_token("wss://app.example", &t.access).await?;

    // 7. revoke = one record write; next refresh fails, access dies within TTL
    //    (done by an admin / server fn: Session.revoked = true)
    Ok(())
}
```

## What each step shows

| Step | Concept                                                                                                                                                                             |
| ---- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1    | Unauthenticated tier — `user = U48::MAX`, login + public reads only.                                                                                                                |
| 2    | `login` is a `#[server]` fn: verifies a credential (local Argon2 **or** OAuth), creates the `Session` record, mints the **access + refresh** pair.                                  |
| 3    | The node derives identity from the **access token**, never the request body.                                                                                                        |
| 4    | Typed CRUD: `create` (versioned, history-chained), `get` (single-lookup Unique anchor).                                                                                             |
| 5    | Filtered/derived reads are server functions — the body runs node-side with DB access; the client ships a `Wire` call.                                                               |
| 6    | Short access TTL lapses → the **refresh** token mints a new access token and **rotates** itself (replay of the old one ⇒ theft signal).                                             |
| 7    | **Revocation** = mark the `Session` record `revoked` (one write). The next `refresh` is refused and the live access token dies within one short TTL — no per-request session store. |
