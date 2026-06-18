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
| `metadata`   | `Metadata` — modification chain, authorship, permission ref.         |
| `hooks`      | `first_try` (pre-search) and `fallback_not_found` (post-miss) hooks.  |
| `permission` | `PermissionRef` shapes.                                              |
| `wire`       | The `Wire` trait + `WaveWire` (no serde). See `docs/wire_format.md`. |
| `registry`   | `ObjectDescriptor` / `ObjectRegistry` lookup by `STRUCT_HASH`.       |
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
| `SALT`   | 15 bits | Per-type discriminator + collision breaker (layout below).                                           |

### The `SALT` field (15 bits)

The trailing 15 bits both **disambiguate the struct type** (needed when `KEY` is a
timestamp, not a struct hash) and **break collisions** within one
`(KEY, TENANT)`. Layout depends on the shape:

| Shape                          | `SALT[14..0]`                                                |
| ------------------------------ | ------------------------------------------------------------ |
| **Unique**                     | `0` (all 15 bits zero — `KEY` already carries the full hash) |
| **NonUnique / BpTree / Pivot** | `salt(u7)` ‖ `trunc8(STRUCT_HASH)`                           |

`salt` is a fixed or random value the writer supplies; the 8-bit `STRUCT_HASH`
truncation co-locates same-type records and tells the engine which type a
timestamp-keyed `Id` belongs to.

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

### `CREATED_AT` time base

`CREATED_AT` is a **nanosecond** count from a fixed WaveDB epoch (a `const`
Rust reference instant). Fine precision keeps collisions rare; `SALT` breaks any
remaining tie within the same nanosecond and tenant.

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

A `u64` identity computed at **compile time** by `#[wavedb]` (see
[`wavedb-macros`](../wavedb-macros/README.md)) as a `const` hash of:

```
STRUCT_NAME + SHAPE + each PROPERTY_NAME + each PROPERTY_TYPE
```

Folding field names and types into the hash means **any schema change yields a
new `STRUCT_HASH`** — a changed struct is simply a different type. There is no
version counter; bridging old and new is done with the lookup hooks below.

An 8-bit truncation of `STRUCT_HASH` rides in the `SALT` field of every
timestamp-keyed ID (NonUnique, BpTree, Pivot — see _The `SALT` field_),
co-locating same-type records and identifying the type when `KEY` is a timestamp.

---

## Metadata

```rust
pub struct Metadata {
    pub old_modification_id: u128, // previous version (0 = first)
    pub new_modification_id: u128, // next version (0 = live)
    pub user: U48,                 // who wrote this version (48-bit newtype)
    pub device_created: u64,       // which device produced it
    pub permission: Option<PermissionRef>, // access rule; None = tenant-only
}
```

No `struct_version` field — the stored record's `STRUCT_HASH` (carried in the
wire envelope) already says which schema it was written under.

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

There is **no query expression tree**. Reads are: a Unique `get`, a NonUnique
collection walk through its `Pivot` → `BpTree` (ordered by `CREATED_AT`), or a
**server function** for anything filtered or derived — an `async fn` that runs on
the node with DB access and is called by a typed client binding (see
[`wavedb-macros`](../wavedb-macros/README.md#server-functions--server)).

---

## Registry

`ObjectRegistry` maps a `STRUCT_HASH` to its `&'static ObjectDescriptor` (field
offsets, heapable flags, heap-prop name list, shape). Built at compile time by
`declare_objects!` (see [`wavedb-macros`](../wavedb-macros/README.md)); lookups
are a const-compare chain — no `dyn`, no runtime registration.
