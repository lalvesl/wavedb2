# wavedb-core

Core primitives shared by **every** node kind and by proc-macro generated code:
the composite `Id`, `STRUCT_HASH`, `Metadata`, the migration registry,
permission refs, the `Wire` serialization trait, and the query expression tree.
**No I/O** — safe in WASM, in macros, everywhere.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module       | Responsibility                                                  |
| ------------ | -------------------------------------------------------------- |
| `id`         | The 128-bit composite `Id` and its field accessors.            |
| `metadata`   | `Metadata` — modification chain, authorship, permission ref.   |
| `migration`  | Migration registry, chain traits, `MigrationChain` read path.  |
| `permission` | `PermissionRef` shapes.                                        |
| `wire`       | The `Wire` trait + `WaveWire` (no serde). See `docs/wire_format.md`. |
| `query`      | `Expr` / `Value` / `Field` query expression tree.             |
| `registry`   | `ObjectDescriptor` / `ObjectRegistry` lookup by `STRUCT_HASH`. |
| `traits`     | `WaveDbStruct`, shape markers.                                 |
| `error`      | Workspace error type.                                          |

---

## The ID

Every record has a composite ID of exactly **128 bits**. The **key is the most
significant field**, so a numeric ordering of the `u128` _is_ an ordering by key
— for the timestamp-keyed shapes that means chronological order, which is what
the `BpTree` indexes on.

```
[ KEY (u64) | TENANT (u48) | FLAG (1) | SALT (7) | reserved (8) ]
   MSB ───────────────────────────────────────────────────── LSB
```

| Field      | Type   | Description                                                                                          |
| ---------- | ------ | ---------------------------------------------------------------------------------------------------- |
| `KEY`      | `u64`  | `STRUCT_HASH` when `FLAG = 1` (Unique anchor), or a `CREATED_AT` timestamp when `FLAG = 0`.           |
| `TENANT`   | `u48`  | Owning tenant. `0` reserved for the system; `U48::MAX` for unauthenticated sessions. B2C: = user id. |
| `FLAG`     | 1 bit  | `1` ⇒ `KEY` is a struct-hash key; `0` ⇒ `KEY` is a `CREATED_AT` timestamp.                            |
| `SALT`     | 7 bits | Collision breaker within a single `(KEY, TENANT)`: writer supplies a fixed or random value.           |
| _reserved_ | 8 bits | Unused — see the bit-budget note in the root README's status.                                        |

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
new `STRUCT_HASH`**. That is the migration boundary — migration is defined as a
transform from one `STRUCT_HASH` to another, with no separate version counter.

The low 7 bits of `STRUCT_HASH` (`trunc7`) are reused as the `SALT`/disambiguator
for NonUnique and BpTree IDs, co-locating same-type records.

---

## Metadata

```rust
pub struct Metadata {
    pub old_modification_id: u128, // previous version (0 = first)
    pub new_modification_id: u128, // next version (0 = live)
    pub user: u48,                 // who wrote this version
    pub device_created: u64,       // which device produced it
    pub permission: Option<PermissionRef>, // access rule; None = tenant-only
}
```

No `struct_version` field — the stored record's `STRUCT_HASH` (carried in the
wire envelope) tells the engine which schema it was written under, so the
migration walk needs nothing extra in `Metadata`.

---

## Schema migration & lazy upgrade

Clients, servers, and nodes may run **different builds simultaneously**. On read,
if a record's stored `STRUCT_HASH` differs from the reader's compiled head, the
migration transform runs in memory and the upgraded record is written back **in
the background** — partial, progressive, no global lock, no maintenance window.
Old data is never a backup-and-restore migration; it just upgrades the next time
it is touched.

---

## Migrations

**One model.** Each struct declares how it relates to its **immediate neighbour
struct hashes** via TYPE paths plus async forward/rollback fns; the macro
reconstructs the whole chain at compile time.

### Neighbour attributes

| Attribute               | Direction | Kind     | Signature / value                              |
| ----------------------- | --------- | -------- | ---------------------------------------------- |
| `migrate_from`          | backward  | type     | The predecessor struct.                        |
| `migrate_from_with`     | backward  | async fn | `async fn<Db>(&Db, Old) -> Result<Self>`       |
| `migrate_rollback`      | forward   | type     | The successor (this struct receives rollback). |
| `migrate_rollback_with` | forward   | async fn | `async fn<Db>(&Db, New) -> Result<Self>`       |
| `first_try`             | —         | async fn | `async fn<Db>(&Db) -> Result<Option<Old>>` — runs **before** the DB search. |
| `fallback_not_found`    | —         | async fn | `async fn<Db>(&Db) -> Result<Option<Self>>` — runs **after** a `None`.      |

**Chain bounds:** the oldest struct has no `migrate_from`; the current head has
no `migrate_rollback`; every middle struct declares both. Rollback co-locates
with the **older** struct ("I know how to receive my future self and become
me").

### Compile-time chain

| Trait                          | Reads as                            | Walks    |
| ------------------------------ | ----------------------------------- | -------- |
| `MigratesFrom { type Source }` | "I migrate from `Source`"           | backward |
| `RollbackFrom { type Future }` | "I receive rollbacks from `Future`" | forward  |

Reaching a chain end is a compile error — the type system **is** the chain-bound
check, so the registry is reconstructable from types alone.

### Cross-hash read

Every struct gets `impl<Db> MigrationChain<Db>`. `read_as_self(db, bytes,
stored_struct_hash)`:

- `stored == Self::STRUCT_HASH` → wire-decode directly.
- stored is an older neighbour → recurse as `Self::Source`, then migrate forward.
- stored is a newer neighbour → recurse as `Self::Future`, then roll back.

Works **without** any runtime `register_migration` call; the wire envelope's
`STRUCT_HASH` tells the engine which way to walk.

### Compose & split

`first_try` covers both: **compose** synthesises a source from several records
before the search; **split** points several new structs' `first_try`s at one old
record, each lifting its slice. Same pipeline, opposite ends.

### Rollback during mixed-build deployments

A node on an older build walks the registered **backward** edges to bring a
newer record down to a hash it can read. Forward + backward must both exist for
two adjacent struct hashes to coexist live. Code-side rollback is a one-line
`pub type` edit; the DB keeps both readable while the `migrate_rollback_with` fn
is compiled in.

---

## Permissions

Access control is stored **inline in `Metadata`**, scoped per record:

| Value                                  | Semantics                                                | Wire cost |
| -------------------------------------- | -------------------------------------------------------- | --------- |
| `None`                                 | Tenant-only — the owning tenant's users (common case).   | 1 byte    |
| `Some(PermissionRef::Public)`          | World-readable.                                          | 1 byte    |
| `Some(PermissionRef::Tenants(list))`   | A specific list of other tenant ids.                    | 1 + list  |
| `Some(PermissionRef::Group(group_id))` | Reference to a shared permission group _(deferred)_.     | 1 + ref   |

A grant is what lets a user of one tenant act on another tenant's data; without
it, tenants never see each other's records.

---

## Query expressions

`Expr` / `Value` / `Field` form the typed query tree evaluated node-side over
descriptor offsets. `Value` covers every numeric width (`U8`…`U128`, `I8`…`I128`,
`F32`, `F64`) plus `Str`/`Bool`/`Bytes`; `From` impls are exact-width. The macro
generates one typed `Field` per column so a misspelt field is a compile error,
not an empty result.

---

## Registry

`ObjectRegistry` maps a `STRUCT_HASH` to its `&'static ObjectDescriptor` (field
offsets, heapable flags, heap-prop name list, shape). Built at compile time by
`declare_objects!` (see [`wavedb-macros`](../wavedb-macros/README.md)); lookups
are a const-compare chain — no `dyn`, no runtime registration.
