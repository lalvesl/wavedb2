# wavedb-core

Core primitives shared by **every** node kind and by proc-macro generated code:
the composite `Id`, `STRUCT_HASH`, `Metadata`, the schema-evolution lookup hooks,
permission refs, and the `Wire` serialization trait.
**No I/O** — safe in WASM, in macros, everywhere.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module       | Responsibility                                                       |
| ------------ | -------------------------------------------------------------------- |
| `id`         | The 128-bit composite `Id`, the `U48` newtype, and field accessors.  |
| `local_id`   | `LocalId` — compact 80-bit ID (no `TENANT`) for BpTree-internal use. |
| `metadata`   | `Metadata` — modification chain, pivot back-link, authorship, permission ref. |
| `hooks`      | `first_try` (pre-search) and `fallback_not_found` (post-miss) hooks. |
| `permission` | `PermissionRef` shapes.                                              |
| `wire`       | The `Wire` trait + `WaveWire` (no serde). See `docs/wire_format.md`. |
| `registry`   | `ObjectDescriptor` / `ObjectRegistry` lookup by `STRUCT_HASH`.       |
| `store`      | The `Store` backend trait (key→value over `Id` + atomic batch).      |
| `index`      | `Pivot`, `BpTree`, `IndexKey`, `Bound` — the `Store`-generic index contracts. |
| `traits`     | `WaveDbStruct`, shape markers.                                       |
| `error`      | Workspace error type.                                                |

---

## The ID

Every record has a composite ID of exactly **128 bits**. The **key is the most
significant field**, so a numeric ordering of the `u128` _is_ an ordering by key
— for the timestamp-keyed shapes that means chronological order, which is what
the `BpTree` indexes on.

```
[ KEY (u64) | TENANT (u48) | FLAG (1) | SALT (15) ]
   MSB ──────────────────────────────────────── LSB
```

| Field    | Type    | Description                                                                                          |
| -------- | ------- | ---------------------------------------------------------------------------------------------------- |
| `KEY`    | `u64`   | `STRUCT_HASH` when `FLAG = 1` (Unique anchor), or a `CREATED_AT` timestamp when `FLAG = 0`.          |
| `TENANT` | `u48`   | Owning tenant. `0` reserved for the system; `U48::MAX` for unauthenticated sessions. B2C: = user id. |
| `FLAG`   | 1 bit   | `1` ⇒ `KEY` is a struct-hash key; `0` ⇒ `KEY` is a `CREATED_AT` timestamp.                           |
| `SALT`   | 15 bits | Collision breaker within one `(KEY, TENANT)`.                                                        |

### The `SALT` field (15 bits)

The trailing 15 bits **only break collisions** within a single `(KEY, TENANT)`:

| Shape                          | `SALT[14..0]`                              |
| ------------------------------ | ------------------------------------------ |
| **Unique**                     | `0` (the fixed anchor needs no salt)       |
| **NonUnique / BpTree / Pivot** | 15 bits of writer-supplied random/fixed value |

There is **no struct-hash truncation in the `Id`**. The type is always known
without it — every lookup is scoped to one `STRUCT_HASH` directory (storage) or
carried in the wire envelope, and the 48-bit `TENANT` already separates tenants.
15 random bits plus the nanosecond `KEY` make a same-tick collision within one
tenant astronomically unlikely; this also keeps the in-memory KV cache's key
space clean (no 256-bucket truncation aliasing).

> **Future.** The 15-bit `SALT` may be masked per connected user to shrink the
> KV-cache collision probability further.

### `U48` — the 48-bit newtype

Rust has no native `u48`, so the 48-bit `TENANT`/`user` values are a newtype
wrapping a `u64`:

```rust
pub struct U48(u64); // invariant: value < 2^48
```

It range-checks on construction, exposes the `U48::MAX` (unauthenticated) and `0`
(system) sentinels, and packs into the `Id`/`Metadata` layout as exactly 48 bits.
Accessors such as `.tenant_id()` and `Metadata::user` return `U48`, never a raw
`u64`. Everywhere this README writes `u48`, the Rust type is `U48`.

### `CREATED_AT` time base — the guarantee

`CREATED_AT` is a **nanosecond** count from a fixed WaveDB epoch (a `const` Rust
reference instant). What is and isn't guaranteed:

- **Uniqueness (guaranteed):** the `Id` is unique within a `(KEY, TENANT)` because
  of the **15-bit random `SALT`**, *not* because of clock monotonicity. Two writes
  in the same nanosecond — or even with the clock running backwards (NTP step) —
  get distinct `Id`s from the salt. At low scale 15 random bits are ample; a
  64-write burst in one ns has a sub-1e-15 collision chance.
- **Ordering (best-effort):** sorting by `CREATED_AT` is approximately
  chronological — good enough for the `BpTree`'s time index — but it is **not a
  strict total order** under clock skew/adjustment. WaveDB does not promise that
  `Id` order equals real-time order across NTP corrections.

> **Future.** At higher scale the `SALT` becomes a **per-user-session mask** (a
> chunk reserved per connected session) so concurrent sessions can't collide and
> the in-memory KV cache stays clean — uniqueness by construction, not just luck.

### Why the `FLAG` bit

It lets one `Id` type encode two addressing schemes in the same `KEY` slot:

- **Unique anchor** — `FLAG = 1`, `KEY = STRUCT_HASH`, `SALT = 0`: a directly
  computable, single-record address. One lookup, no index.
- **Everything timestamped** (NonUnique records, Pivots, BpTree nodes) —
  `FLAG = 0`, `KEY = CREATED_AT`: chronologically ordered, indexable by `BpTree`.

There is **no `STRUCT_ID`** and **no schema-version field** in the ID. Type
identity and schema generation are both folded into `STRUCT_HASH`.

---

## `STRUCT_HASH`

A `u64` identity computed at **build time** (in `wavedb-build` / `#[wavedb]`, see
[`wavedb-macros`](../wavedb-macros/README.md)) over the canonical string:

```
STRUCT_NAME + SHAPE + each PROPERTY_NAME + each PROPERTY_TYPE
```

**Algorithm: `ahash` with a fixed, hard-coded seed.** The seed is a compile-time
constant baked into the tooling — *not* the random per-database seed — so the
hash is **deterministic across every build and machine** (clients and servers
must agree on a type's identity). ahash is fast and gives good u64 dispersion.

Folding field names and types into the hash means **any schema change yields a
new `STRUCT_HASH`** — a changed struct is simply a different type. There is no
version counter; bridging old and new is done with the lookup hooks below. The
`STRUCT_HASH` does **not** appear inside the `Id`'s `SALT`; the type is known from
the per-`STRUCT_HASH` storage directory and the wire envelope.

---

## Metadata

```rust
pub struct Metadata {
    pub old_modification_id: Option<LocalId>, // None = first version
    pub new_modification_id: Option<LocalId>, // None = live record
    pub pivot_id: Option<LocalId>,            // None = Unique record
    pub user: U48,                            // who wrote this version (48-bit newtype)
    pub device_created: u64,                  // which device produced it
    pub permission: Option<PermissionRef>,    // access rule; None = tenant-only
}
```

No `struct_version` field — the stored record's `STRUCT_HASH` (carried in the
wire envelope) already says which schema it was written under.

Modification IDs and `pivot_id` use `Option<LocalId>` (80-bit when `Some`)
instead of a full `Id` (128-bit): the BpTree is already tenant-scoped so the
48-bit `TENANT` is redundant, and `Option<T>` costs only 1 stack byte (flag) with
the `LocalId` payload on the heap only when `Some`. Stack size = **18 bytes**
(3 × 1-byte flag + 6 user + 8 device + 1 permission flag). A Unique record with
all three `None` has zero heap bytes for those fields.

### `LocalId` — 80-bit compact ID

```
[ KEY (u64) | FLAG (1) | SALT (15) ]
   MSB ─────────────────────────── LSB
```

`LocalId` is `Id` with `TENANT (u48)` stripped — 10 bytes on the wire. The BpTree
is already scoped to a tenant, so `TENANT` is derivable from context. Reconstruct
a full `Id` with `local_id.to_id(tenant)` — two or three CPU cycles, in memory,
never disk.

### `pivot_id` — the NonUnique reindex back-link

A NonUnique `save` (update) **force-reindexes every live tree** of its collection —
the `current` `BpTree` *and* every `#[wavedb::pivot(...)]` secondary — so it must
reach all the tree roots, which live in the collection's **`Pivot`**. The record
therefore carries its owning `PivotId` here as a `LocalId` (the typed
`<T>::PivotId` is the compile-time view only — core never names macro types).

- **Stamped at `insert`** from the collection handle's `PivotId`; `None` for Unique.
- Lets `save` reindex from the record alone, without re-passing the handle.
- It is **outside `STRUCT_HASH`** (`name + shape + field names + types`), so it
  changes **no** struct's identity — only Metadata's own wire layout.

Why not the `dead` tree on update? Because history is the `old_modification_id` ↔
`new_modification_id` chain above: the previous version is retained and linked, so
update never writes `dead`. `dead` is populated **only** by `remove`. The record's
identity `Id` (the insert anchor) stays stable across updates so references never
break; the trees re-establish the live version against that anchor.

---

## Schema evolution — lookup hooks

There is **no migration chain, no rollback graph, no auto-upgrade walk.** A
schema change just yields a new `STRUCT_HASH`; old records keep their old hash.
Bridging that transition is the application's job, through two optional async
hooks declared on a struct:

| Hook                 | Runs                        | Signature                                   |
| -------------------- | --------------------------- | ------------------------------------------- |
| `first_try`          | **before** the DB search    | `async fn<Db>(&Db) -> Result<Option<Self>>` |
| `fallback_not_found` | **after** the search misses | `async fn<Db>(&Db) -> Result<Option<Self>>` |

- **`first_try`** lets you produce the value before touching storage — e.g.
  synthesise it from records stored under a previous `STRUCT_HASH`, or
  short-circuit a known case.
- **`fallback_not_found`** runs only when the normal lookup returns `None` — the
  place to fetch, derive a default, or lift an old record forward.

Both are plain functions you write; the engine simply calls them at those two
points in the read path. Nothing else about versioning is built in.

---

## Permissions

Access control is stored **inline in `Metadata`**, scoped per record:

| Value                                  | Semantics                                              | Wire cost |
| -------------------------------------- | ------------------------------------------------------ | --------- |
| `None`                                 | Tenant-only — the owning tenant's users (common case). | 1 byte    |
| `Some(PermissionRef::Public)`          | World-readable.                                        | 1 byte    |
| `Some(PermissionRef::Tenants(list))`   | A specific list of other tenant ids.                   | 1 + list  |
| `Some(PermissionRef::Group(group_id))` | Reference to a shared permission group _(deferred)_.   | 1 + ref   |

A grant is what lets a user of one tenant act on another tenant's data; without
it, tenants never see each other's records.

> The cross-tenant **serving path** (which node serves tenant A's data to a
> tenant-B user, and where the grant is enforced across nodes) is **deferred** —
> the model above is the contract; the multi-node routing is not built yet.

There is **no query expression tree**. Reads are: a Unique `get`, a NonUnique
collection walk through its `Pivot` → `BpTree` (ordered by `CREATED_AT`), or a
**server function** for anything filtered or derived — an `async fn` that runs on
the node with DB access and is called by a typed client binding (see
[`wavedb-macros`](../wavedb-macros/README.md#server-functions--server)).

---

## Registry

`ObjectDescriptor` carries a type's static shape (field offsets, heapable flags,
heap-prop name list, shape). The **registry that maps a `STRUCT_HASH` to its type
is generated in `build.rs`**, not here: a scanner walks the schema crate, finds
every `#[wavedb]` struct, and emits a generated `Object` enum
(`STRUCT_HASH` → variant) spliced in with
`include!(concat!(env!("OUT_DIR"), …))`. Dispatch — wire-parse, the `first_try` /
`fallback_not_found` hooks, the generated `Pivot`/`BpTree` accessors — is a
`match` on that enum: **no `dyn`, no runtime registration**. The mechanism lives
in [`wavedb-macros` § the registry](../wavedb-macros/README.md#the-registry--generated-in-buildrs).

---

## How a call runs: local store + network

The developer only ever writes the typed call — `unique_var.save(&db)`,
`T::get(&db)`, the collection methods. Each one does **two things internally**,
and nothing else leaks:

1. **write-through to the local store** — a key→value map of `Id → wire bytes`
   kept on the client. Gives warm reads and survives reconnects.
2. **send to the owner node** over `wavedb-net` — the node is the **authoritative
   writer**: it runs the real engine (confirm `Id`, pages, `Pivot`/`BpTree`,
   history, replication) and acks.

```rust
unique_var.save(&db).await?;   // ← the only thing the app writes

// inside (lives in `wavedb`, over the Db handle):
let bytes = unique_var.to_wire();      // [STRUCT_HASH][Metadata][body]
db.local().update(id, &bytes).await?;   // 1. local write-through (Store)
db.send(Op::Save, bytes).await?;        // 2. network → node
```

The `Id` is known client-side without a round-trip: a Unique anchor is
deterministic (`STRUCT_HASH · TENANT · 1 · 0`); a NonUnique record's identity `Id`
is minted at `insert` (`CREATED_AT` + tenant + random `SALT`). So the
write-through happens immediately; the node confirms.

## `Store` — the local backend trait

`Store` is the **backend seam** — the only thing that differs native vs web. Core
declares the contract (async, no I/O — WASM/macro-safe). It is key→value over `Id`
plus an **atomic batch** (`apply`) so a multi-record op (record + `BpTree` node)
commits all-or-nothing:

```rust
pub trait Store {                  // key→value over Id + wire bytes
    async fn get(&self, id: Id) -> Result<Option<Vec<u8>>>;
    async fn apply(&self, batch: &[Write]) -> Result<()>;  // atomic: all-or-nothing
}
pub enum Write { Put(Id, Vec<u8>), Remove(Id) }
```

- **native node** impl: the page engine (`wavedb-storage`) — `apply` = one journal
  entry + cache; durability + atomicity.
- **web** impl: IndexedDB — `apply` = one IDB readwrite transaction.
- **native client** impl: a file-backed key→value store.

The pages/journal/allocator live **behind** this trait, inside the native page
`Store`. The `Pivot`/`BpTree` logic sits **above** it (the `index` contracts below)
and is `Store`-generic — so the **same index code runs on the node (PageStore) and
on web (IndexedDB)**; only the backend swaps.

**Reads.** `get(id)` checks the local `Store` first; on a miss it fetches from the
node and back-fills. Collection queries (`all` / `by_field`) need the `BpTree` —
which is `Store`-generic, so it runs **wherever the engine is linked**: on the node
(PageStore) for a thin client, or **in-browser over IndexedDB** for a serverless app.

## Index contracts — `Pivot`, `BpTree`, `IndexKey`

The index lives **above** `Store` and depends only on it (`get` + `apply`), so it is
**portable** — pages and journal are `PageStore` internals, never named here. Core
declares the contracts; the same code compiles for the node (PageStore) and the web
(IndexedDB).

```rust
/// Order-preserving key encoding: byte order == value order, so the `BpTree`
/// compares keys by memcmp with no decode. Macro-implemented per indexed type
/// (u64 → big-endian, i64 → sign-flipped, String → 0x00-terminated, tuples → concat).
pub trait IndexKey {
    fn encode_key(&self, out: &mut Vec<u8>);
}

/// The collection's roots holder. `#[wavedb]` generates one per NonUnique type;
/// this trait is the portable shape the engine reads. No element counter — the
/// `Pivot` is rewritten only when a `BpTree` root moves.
///
/// Root pointers are `LocalId` (80-bit): the tree is tenant-scoped so `TENANT`
/// is derivable from context — no need to repeat 6 bytes per pointer.
pub trait Pivot: Wire + Sized {
    fn current(&self)     -> LocalId;     // living-records B+tree root
    fn dead(&self)        -> LocalId;     // deleted-records B+tree root
    fn secondaries(&self) -> &[LocalId];  // one root per `#[wavedb::pivot(...)]`
}

/// A search bound over the order-preserving key space.
pub enum Bound {
    All,
    Exact(Vec<u8>),
    Range { lo: Vec<u8>, hi: Vec<u8> },  // half-open [lo, hi)
    Prefix(Vec<u8>),
}

/// A B+tree index over any `Store`. Nodes are records read by `LocalId`; all I/O
/// is delegated to `Store`, so one impl serves native PageStore and web IndexedDB.
/// `search` returns full record `Id`s (two-phase: index → `Id`s → fetch).
pub trait BpTree<S: Store>: Sized {
    fn at(root: LocalId) -> Self;                               // open a tree at a root

    fn search(&self, store: &S, bound: Bound)
        -> impl Stream<Item = Result<Id>>;                      // walk → matching record Ids

    async fn insert(&self, store: &S, key: &[u8], id: Id) -> Result<LocalId>; // → (maybe moved) root
    async fn remove(&self, store: &S, key: &[u8], id: Id) -> Result<LocalId>; // → (maybe moved) root
}
```

`insert`/`remove` take a record `Id` (full 128-bit — external address) and return
the (possibly moved) root as `LocalId` (tree-internal pointer, tenant stripped).
When a root moves the holder rewrites the `Pivot`, otherwise the `Pivot` stays
immutable. Comparison is `memcmp` on the `IndexKey`-encoded bytes
(== `Ordering::{Less,Equal,Greater}`), never a typed decode.

### 32 KiB pages — fanout and I/O

BpTree pages are **32 KiB** (8 × 4 KiB blocks), **one node per page**. Both
node kinds use the same 18-byte entry format:

```
entry = [ key: [u8; 8] ][ LocalId: 10 bytes ]
```

- **Internal node**: `LocalId` = child BpTree page pointer.
- **Leaf node**: `LocalId` = NonUnique record pointer.

All `LocalId`s inflate to full `Id` via `local_id.to_id(tenant)` — 2–3 CPU
cycles, never disk.

Usable bytes per page: `32 768 − 20 (header) ≈ 32 748`. Per-entry cost = **18
bytes**. Capacity ≈ **1 819 entries per page**. Tree height in page reads:

| Records   | Page reads |
| --------- | ---------- |
| ≤ 1 819   | 1          |
| ≤ 3.31 M  | 2          |
| ≤ 6.03 B  | 3          |

See [`wavedb-storage`](../../crates/wavedb-storage/README.md#bptree-page-layout--32-kib-one-node-per-page)
for the full page layout, capacity math, split algorithm, and merge policy.

### Composite — set algebra on `Id` streams

Combining indexes (the no-DSL composite query) is free functions over the `Id`
streams `search` yields — `Store`-agnostic, so they too run native or web:

```rust
pub trait IdStreamExt: Stream<Item = Result<Id>> + Sized {
    fn intersect<S>(self, other: S) -> Intersect<Self, S>; // AND
    fn union<S>(self, other: S)     -> Union<Self, S>;     // OR  (dedup)
    fn except<S>(self, other: S)    -> Except<Self, S>;    // NOT (difference)
}
```

Streams from different indexes arrive in different orders, so `intersect`/`except`
buffer the **smaller** side into an `Id` set and probe the larger; `union` merges +
dedups. A `#[server]` body composes these, then resolves survivors with a fetch.

## Typed object traits (per struct, macro-implemented)

The calls above are surfaced as **typed traits, one per shape**, that `#[wavedb]`
implements for each struct. The methods route through the `Db` handle (which owns
the local `Store` and the transport) — they never expose `Store` or the network to
app code.

```rust
// Unique
pub trait UniqueObject: WaveDbStruct {
    async fn get(db: &Db) -> Result<Option<Self>>;   // local → (miss) node
    async fn save(&self, db: &Db) -> Result<()>;     // local write-through + send
}

// NonUnique — collection ops open a handle from a stored PivotId.
pub trait NonUniqueObject: WaveDbStruct + Sized {
    fn collection(db: &Db, pivot: PivotId) -> Collection<Self>;
    async fn save(&self, db: &Db) -> Result<()>;     // update at identity Id (local + send)
}

impl<T: NonUniqueObject> Collection<T> {
    async fn insert(&self, db: &Db, record: T) -> Result<Id>;  // mint Id, local + send
    async fn get(&self, db: &Db, id: Id) -> Result<Option<T>>;
    fn all(&self, db: &Db) -> impl Stream<Item = Result<T>>;   // node-side BpTree walk, streamed
    async fn remove(&self, db: &Db, id: Id) -> Result<()>;
    // + a `by_<field>` lookup per `#[wavedb::pivot(...)]` secondary index, also a Stream.
}
```

**Collection reads are async iterators.** `all` and every generated `by_<field>`
return `impl Stream<Item = Result<T>>`, never a buffered `Vec`: the two-phase
BpTree walk (index → `Id`s → fetch) streams records as it resolves each `Id`, so a
caller can stop early without materialising the whole collection. Use
`.try_collect().await?` when a `Vec` is actually wanted. The prelude re-exports
`Stream` / `StreamExt`.

The macro picks `UniqueObject` vs `NonUniqueObject` by shape; `db: &Db` is the
macro's generic `__WaveDbDb` parameter, resolved at the call site (no `dyn`). The
same typed code compiles native and wasm — only the local `Store` impl swaps.
