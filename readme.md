# 🌊 WaveDB — A User-Data-Partitioned, Tenant-Centric, Full-Stack Database

> _"Technology works like ocean waves — going back and forth, but always advancing toward the shore (global-warm)."_

---

## What WaveDB is

WaveDB is a **database shipped as a Rust crate** — you add it as a library and
compile it _into_ your application. It is **full-stack**: the exact same code and
schema run **client-side** (native apps and browser/WASM) and **server-side**
(the storage nodes), so there is no separate API/DTO/ORM layer to keep in sync —
the schema crate _is_ the protocol.

Four properties define it:

1. **Tenant-partitioned data ownership.** Every record belongs to a tenant. A
   tenant is the unit of write-ownership across the cluster.
2. **Horizontal distribution & redundancy.** More than one node serves and
   stores a user's data; ownership of writes is assigned by tenant (and, later,
   by `STRUCT_HASH`), so scaling out and replicating is structural, not bolted
   on.
3. **Timeline / history as a first-class citizen.** Saving never destroys the
   old bytes — every modification is tracked so you can navigate a record's
   timeline and read its past states.
4. **Schema evolution by `STRUCT_HASH`.** A struct's identity is a hash of its
   shape, so changing it just makes a new type; clients, servers, and nodes can
   run different builds at once. Bridging old and new data is done with two
   application hooks (`first_try` / `fallback_not_found`) — no global migration
   step, no backup-and-restore.

On top of that: a **permission manager** for sharing data between users and
tenants (tenant-only / public / a specific list of tenants / a group — groups
deferred), and a storage layer that adapts to its compile target — **native**
builds (Android, Windows, Linux, macOS, iOS) use the filesystem (`data.bin` +
journal); **web** builds use **IndexedDB directly** (no journal needed).

> This file is the **idea + quickstart**. Every detailed mechanism lives in the
> crate that implements it — see the [Crate map](#crate-map).

---

## The Core Idea

### The problem with traditional SQL (for applications)

Conventional SQL organises storage **by type — not by who owns the data or what
it relates to**. Two kinds of unrelated data end up physically interleaved on
the same pages, or the engine has to navigate between leaf nodes to find the
right id, for the right user, with the right relation to another table — and
application reads pay for it twice or more as the joins compound. _ONE QUERY, 10
JOINS._ WaveDB explores serving an application read with **one disk IO**, without
tens of GB of cache papering over the layout.

#### Mixing 1 — every tenant's rows share one table

```sql
-- Every tenant's orders share one table, so the partition key
-- ("whose data?") has to ride in every query next to the real filter.
SELECT * FROM orders
WHERE user_id = 42      -- partition key — restated on every read
  AND amount > 100;     -- the business filter you actually wanted
```

The `orders` table holds **every user's orders**, interleaved row-by-row across
shared pages. To serve one user the engine index-walks or scans through bytes
belonging to thousands of unrelated tenants, then filters down to the few rows
that matter. Every page pulled into the buffer pool is mostly data the query
throws away.

In WaveDB the partition key is **structural, not a predicate**: the tenant is
bound once at connect and never appears in a query again — only the business
filter is left.

#### Mixing 2 — unrelated child rows share one table (the nested case)

Even inside a single tenant, a classic schema puts every invoice's line items in
one `invoice_lines` table:

```sql
SELECT * FROM invoice_lines WHERE invoice_id = 1001;
```

The lines for invoice `1001` sit scattered among the lines of every other
invoice — and, combined with Mixing 1, every other tenant's invoices too. The
rows you actually want (one parent's children) are never colocated on disk.
Fetching them is a scatter of random page reads guided by an index, never one
sequential pull.

#### The shared root cause

Both problems stem from laying out storage **by table/type instead of by access
pattern**. Application reads are almost always _"give me this one tenant's — and
often this one parent's — data."_ But the bytes that answer that question are
sprayed across pages shared with data nobody asked for. The cost surfaces as
**cache misses and wasted disk IOPS**.

**WaveDB starts from that endpoint.** Data is partitioned by tenant from the
first byte, and a collection's members are reached through a per-collection index
(`Pivot` → `BpTree`) so the bytes a query needs sit together. The CPU saved from
join processing goes to compression instead.

### The ocean wave analogy

| Era     | Web Frontend                                          | Database                                       |
| ------- | ----------------------------------------------------- | ---------------------------------------------- |
| Past    | Static pages (fast, not dynamic)                      | DB and server tightly coupled                  |
| Present | Client-side rendering (dynamic, slow)                 | Independent DBs with ORMs as glue              |
| Future  | Server-side dynamic rendering (Next.js, Nuxt, Leptos) | **Tenant-partitioned, application-centric DB** |

The cycle closes. Each iteration looks like regression but carries forward the
best properties of both worlds — and here the database itself is shipped as the
library that the full-stack app compiles in.

---

## Tenants and users

A **tenant** is an organization — a junction of many users. This is the common
shape for B2B (a company and its employees). For a B2C application the tenant
number **equals** the user number, because the organization has exactly one user:
itself.

- Data of different tenants **never mixes** in any structure (`Unique`,
  `NonUnique`, `Pivot`, `BpTree`).
- A user belonging to one tenant **may read or write another tenant's data — but
  only with permission** (see [Permissions](#permissions)).
- On disk, data is grouped only to make **dictionaries and compression** more
  effective — grouping is a storage optimisation, never a sharing mechanism.

---

## Data Model

### The ID

Every record has a composite ID of **128 bits**. The most significant field is
the **key**, so a numeric sort of the `u128` is a sort by key — which for
timestamp-keyed records is a chronological order, ideal for the `BpTree`.

```
[ KEY (u64) | TENANT (u48) | FLAG (1) | SALT (15) ]
   MSB ──────────────────────────────────────── LSB
```

| Field    | Width   | Meaning                                                                                    |
| -------- | ------- | ------------------------------------------------------------------------------------------ |
| `KEY`    | `u64`   | Either a `STRUCT_HASH` (Unique) **or** a `CREATED_AT` timestamp — disambiguated by `FLAG`. |
| `TENANT` | `u48`   | Owning tenant. For B2C this is the user id.                                                |
| `FLAG`   | 1 bit   | `1` ⇒ `KEY` is a struct-hash key (Unique anchor); `0` ⇒ `KEY` is a `CREATED_AT` timestamp. |
| `SALT`   | 15 bits | Per-type discriminator + collision breaker — see below.                                    |

The 15-bit `SALT` is shape-dependent: **Unique** = `0`; every timestamp-keyed
shape (**NonUnique / BpTree / Pivot**) = `salt(u7) ‖ trunc8(STRUCT_HASH)`. The
8-bit truncated hash co-locates same-type records and identifies the type when
`KEY` is a timestamp.

**`CREATED_AT` is nanosecond precision** measured from a fixed WaveDB epoch
(a constant Rust instant), so the 64-bit timestamp orders records finely; `SALT`
breaks ties if two writes land in the same nanosecond.

There is **no `STRUCT_ID` and no schema-version field** in the ID anymore — type
identity lives in `STRUCT_HASH` (below), which subsumes both.

### `STRUCT_HASH`

Each declared struct gets a `STRUCT_HASH: u64`, computed **at compile time** by
the `#[wavedb]` macro as a `const` hash of:

```
hash( STRUCT_NAME + SHAPE(Unique|NonUnique|NestedNonUnique)
      + each PROPERTY_NAME + each PROPERTY_TYPE )
```

Because the hash folds in field names and types, **any schema change produces a
new `STRUCT_HASH`** — a changed struct is simply a different type. The old model's
separate "struct id + numeric version" is gone; bridging old and new data is done
with the `first_try` / `fallback_not_found` hooks (below).

### Base data types

| Type          | Declared as            | Cardinality                                  | ID layout (`KEY · TENANT · FLAG · SALT`)                |
| ------------- | ---------------------- | -------------------------------------------- | ------------------------------------------------------- |
| **Unique**    | `#[wavedb]` (default)  | Exactly one live record per tenant           | `STRUCT_HASH · TENANT · 1 · 0`                          |
| **NonUnique** | `#[wavedb(NonUnique)]` | Many per tenant; may nest in other NonUnique | `CREATED_AT · TENANT · 0 · salt7‖trunc8(STRUCT_HASH)` |
| **Pivot**     | auto-generated         | One per NonUnique collection (the handle)    | `CREATED_AT · TENANT · 0 · salt7‖trunc8(STRUCT_HASH)`   |
| **BpTree**    | auto-generated         | Index nodes addressing a collection          | `CREATED_AT · TENANT · 0 · salt7‖trunc8(STRUCT_HASH)` |

**Unique** is the default — the everyday "one record per tenant" object:

```rust
#[wavedb]
pub struct AboutUser {
    pub name: String,
    pub surname: String,
    pub phone: String,
    pub address: String,
}
```

Its single live record sits at a **directly computable anchor address**
(`STRUCT_HASH · TENANT · 1 · 0`), so a read is one lookup with no index walk.

**NonUnique** objects exist many times within a tenant and may nest recursively
inside other NonUnique objects. A collection is referenced through a **`PivotId`**
held in a field — you `get().await?` the pivot to navigate into the collection:

```rust
#[wavedb]
pub struct UserInterestedFruits {
    // a handle into a NonUnique collection of Fruit, reached via its Pivot
    pub list_of_fruits: <Fruit as WaveDbStruct>::PivotId,
}
```

**Pivot** (auto-generated by `#[wavedb(NonUnique)]`) is the collection handle.
It carries no business data — only the addressing into the index trees:

```rust
pub struct Pivot {
    pub counter: u64,           // live element count
    pub current: BpTreeId,      // u128 pointer to the B+tree of living records
    pub dead:    BpTreeId,      // u128 pointer to the B+tree of deleted records
}
```

**BpTree** (also auto-generated) is the index itself — a B+tree keyed by
`CREATED_AT`, holding addresses of NonUnique records (not the records' bytes).
There is one tree for living data and one for deleted data.

> Pivot and BpTree are generated _per_ NonUnique type. If two such generated
> types share the same name and field shape their `STRUCT_HASH` may collide —
> that is harmless, because they are only ever addressed within their own
> tenant/collection context.

### Operations

- **Unique** — `get` and `save`. `save` on an existing record updates the live
  anchor in place and chains the previous bytes into history via `Metadata`
  (timeline preserved).
- **NonUnique** — `insert`, `update`, `delete`. Each revalidates the `BpTree`.
  `delete` moves the record from the **current** tree to the **dead** tree
  (nothing is erased — history stays navigable).

### History / timeline

Saving never frees the old bytes. The live record's `Metadata` chains backward
(`old_modification_id`) and forward (`new_modification_id`) so a record's full
timeline is walkable. Deleted NonUnique records live on in the dead `BpTree`.

---

## Permissions

Access control is stored inline in each record's `Metadata`, scoped per record:

| Setting         | Who can access                                            |
| --------------- | --------------------------------------------------------- |
| **Tenant-only** | Only the owning tenant's users (the common case).         |
| **Public**      | World-readable.                                           |
| **Tenant list** | A specific list of other tenants.                         |
| **Group**       | A shared permission group _(deferred — not implemented)_. |

This is the mechanism by which a user of one tenant can act on another tenant's
data: the data's owner grants it.

---

## Schema evolution

A struct change yields a new `STRUCT_HASH` (the old "version number" concept is
gone). There is no migration chain or auto-upgrade walk — bridging old and new
data is done with two optional application hooks: **`first_try`** runs before a
read hits storage (synthesise the value, e.g. from an older `STRUCT_HASH`), and
**`fallback_not_found`** runs after a miss (fetch or derive a default). Details:
[`wavedb-core`](crates/wavedb-core/README.md#schema-evolution--lookup-hooks).

---

## Simple usage

```rust
use wavedb::prelude::*;

// For a B2C app the tenant *is* the user — bound once, at connect time.
let db = Db::connect("wss://wavedb.example", /* user */ 42, /* tenant */ 42).await?;

// Unique: one record per tenant.
let profile: Option<AboutUser> = AboutUser::get(&db).await?;

// NonUnique: list the collection (Pivot → BpTree, time-ordered).
let recent: Vec<Order> = Order::all(&db).await?;

order.save(&db).await?;     // versioned update; old bytes chained into history
order.delete(&db).await?;   // moves to the dead B+tree (NonUnique only)

// Filtered / derived reads are server functions — an async fn that runs on the
// node, called through a generated client binding (no client-side query DSL).
#[server]
async fn orders_over(db: &Db, min: u64) -> Result<Vec<Order>> {
    Ok(Order::all(db).await?.into_iter().filter(|o| o.amount > min).collect())
}
let big: Vec<Order> = orders_over(&db, 100).await?;
```

A whole application backend is one expression — attaching the registry turns a
generic node into _your_ backend:

```rust
declare_objects! { pub mod app_objects { orders: [Order], … } }

Server::bind("0.0.0.0:7700")
    .tenant(42)
    .data_dir("./data")
    .registry(app_objects::REGISTRY)
    .serve()
    .await
```

The full client API and object lifecycle live in
[`wavedb`](crates/wavedb/README.md).

---

## Crate map

| Crate                                                     | What it owns                                                               | Read for                                                                                                           |
| --------------------------------------------------------- | -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| [`wavedb`](crates/wavedb/README.md)                       | Client `Db` handle, typed CRUD, server-fn call bindings                    | Quickstart, entry points, object lifecycle                                                                         |
| [`wavedb-core`](crates/wavedb-core/README.md)             | `Id`, `Metadata`, `STRUCT_HASH`, schema-evolution hooks, permissions, wire | ID layout, struct-hash identity, **schema evolution**                                                              |
| [`wavedb-macros`](crates/wavedb-macros/README.md)         | `#[wavedb]`, `declare_objects!`, auto-generated `Pivot`/`BpTree`           | Object declaration, `STRUCT_HASH` derivation, validate/preprocess                                                  |
| [`wavedb-storage`](crates/wavedb-storage/README.md)       | The per-node engine                                                        | **Block manager, per-`STRUCT_HASH` page directory, linear hashing**, pages, dictionaries, journal + cache pipeline |
| [`wavedb-quick-node`](crates/wavedb-quick-node/README.md) | Serving/storage node                                                       | Tenant write-ownership ring, replication, routing/failover, node-side validation                                   |
| [`wavedb-net`](crates/wavedb-net/README.md)               | Transport                                                                  | WebSocket / HTTP queue, Bloom screen-sync                                                                          |
| [`wavedb-wasm`](crates/wavedb-wasm/README.md)             | Browser client                                                             | IndexedDB key→value storage (no pages, no journal)                                                                 |

Tooling: `wavedb-examples`, `wavedb-bench`, `wavedb-test-cluster`.

A cold/history tier (slow-node) and cluster monitors are **deferred — not the
moment**; their crates are intentionally absent for now.

---

## Non-Goals

- **Not OLAP.** Cross-tenant aggregations belong in a dedicated analytics pipeline.
- **Not a general consensus system.** Consistency is tenant-scoped; multi-tenant eventual consistency is by design.
- **Not a SQL replacement.** No query DSL — reads are `get` / collection walk /
  server functions; there is no ad-hoc cross-table query language.
- **Not offline-first (yet).**

---

## Implementation language

Rust — the `#[wavedb]` proc-macro computes `STRUCT_HASH` and enforces ID/Metadata
structure at compile time. The `Wire` format defines the byte layout explicitly
(no `repr(C)`, no serde — the macro emits the encode/decode), `async` end to end,
and the same source compiles to native (Tokio, filesystem)
and browser (WASM via `wasm_bindgen_futures`, `fetch`, `gloo_net`, IndexedDB).

---

## Status

🔬 **Research / rebuild phase.** The workspace is a clean reimplementation: the
design below is being rebuilt from the ground up. These docs describe the
**target** architecture, not shipped code.
