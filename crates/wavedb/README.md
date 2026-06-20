# wavedb

The user-facing **client library** — the `Db` handle, typed object CRUD, and the
client bindings that call server functions. Same code path on servers, native
clients, and (compiled to WASM) browsers.

> For the project-wide idea see the [root README](../../readme.md). For the
> wire/transport layer see [`wavedb-net`](../wavedb-net/README.md); for object
> declaration see [`wavedb-macros`](../wavedb-macros/README.md).

## Entry points

| Mode                                        | Storage location         | Tenant model                |
| ------------------------------------------- | ------------------------ | --------------------------- |
| `Db::open(url, path, user)`                 | Local file at `path`     | `tenant = user_id`          |
| `Db::open(url, path, user, default_tenant)` | Local file at `path`     | Tenant explicit (companies) |
| `Db::open(url, user, default_tenant)`       | Browser IndexedDB (WASM) | Tenant explicit             |
| `Db::open(url, user)`                       | Browser IndexedDB (WASM) | `tenant = user_id`          |

`url` resolves to the cluster's front door; the request is redirected to the
Quick-Node owning the user's tenant, and the **backup** Quick-Node URL is
returned alongside for failover. Native modes use the local `path` as a
write-through cache under `tokio::broadcast`; WASM modes use IndexedDB in the
same role. A `Db` can spawn another for a different tenant:

```rust
let other = db.another_tenant(other_tenant_id).await?;
```

The `Drop` impl notifies the Quick-Node so the session is released promptly.

## The partition key is structural

The tenant is bound **once, at connect** — you never restate it on a read. The
engine only ever touches the connected tenant's data. The typed methods come from
per-struct traits the `#[wavedb]` macro implements by shape (`UniqueObject` /
`NonUniqueObject`, see below).

```rust
use wavedb::prelude::*;

let db = Db::connect("wss://wavedb.example", /* user */ 42, /* tenant */ 42).await?;

// Unique (UniqueObject): one record per tenant. No create — save is upsert.
let mut profile: AboutUser = AboutUser::get(&db).await?.unwrap_or_default();
profile.city = "Lisbon".into();
profile.save(&db).await?;

// NonUnique (NonUniqueObject): open the collection from a stored PivotId.
let orders = Order::collection(&db, profile.orders); // Collection<Order>, carries the PivotId
let id = orders.insert(&db, Order { amount: 120 }).await?; // assigns identity Id, adds to BpTree
let recent: Vec<Order> = orders.all(&db).await?;           // walk current BpTree → Ids → fetch

let mut o = orders.get(&db, id).await?.unwrap();
o.amount = 130;
o.save(&db).await?;          // update in place at the identity Id; tree untouched
orders.remove(&db, id).await?; // move Id current → dead BpTree (history kept)
```

## Object lifecycle & the typed traits

There is **no `create`** — `save` is an upsert. A Unique `save` writes the live
anchor; a NonUnique record gets its **identity `Id` at `insert`** and keeps it,
so later `save`s rewrite in place and chain history without touching the `BpTree`.

The macro implements one typed trait per shape (defined in
[`wavedb-core`](../wavedb-core/README.md#storage-backend-trait-store)):

| Shape         | Trait              | Methods                                                            |
| ------------- | ------------------ | ----------------------------------------------------------------- |
| **Unique**    | `UniqueObject`     | `T::get(&db)`, `record.save(&db)`                                 |
| **NonUnique** | `NonUniqueObject`  | `T::collection(&db, pivot) → Collection<T>` (`insert`/`get`/`all`/`remove`) + `record.save(&db)` |

Both ride on the low-level `Store` backend, so the same calls work native and on
wasm.

## Filtered / derived reads: server functions

There is **no client-side query DSL**. Anything past "get this" or "list the
collection" — filtering, aggregation, joining across collections, derived views —
is a **server function**: an `async fn` that runs on the node with full DB
access, declared with `#[server]` and called through a generated typed binding
that ships the arguments over the wire. The client awaits it like a local async
fn; the body never enters the client binary.

```rust
// Declared once in the schema crate; the body runs only on the node.
#[server]
async fn orders_over(db: &Db, min: u64) -> Result<Vec<Order>> {
    Ok(Order::all(db).await?.into_iter().filter(|o| o.amount > min).collect())
}

// Client side: a generated binding with the same signature.
let big: Vec<Order> = orders_over(&db, 100).await?;
```

Mechanism (the `#[server]` macro, wire-encoded args, transport) lives in
[`wavedb-macros`](../wavedb-macros/README.md#server-functions--server).

## Everything is `async`

Every public API on `Db`, every storage actor, every evolution hook is `async`
end to end — no blocking IO surface, no hidden thread-pool dispatch. Native runs on
Tokio; the browser on `wasm_bindgen_futures`. The public API is identical.

## Unauthenticated sessions

A client without credentials connects with `user = U48::MAX`. The session sees
only **public data**; the API is restricted to login (local password or OAuth)
and reading world-readable data. Login mints a stateless HMAC **access** token
(short TTL, identity derived node-side) plus a revocable **refresh** token;
structure in [`wavedb-net`](../wavedb-net/README.md#authentication).
