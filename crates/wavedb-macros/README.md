# wavedb-macros

The compile-time front door. `#[wavedb]` turns a plain Rust struct into a WaveDB
object; `declare_objects!` collects objects into a registry every node shares.
All schema rules are written once and enforced on client and server because both
compile this crate.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module              | Responsibility                                                  |
| ------------------- | --------------------------------------------------------------- |
| `lib`               | The `#[wavedb]` and `declare_objects!` entry points.            |
| `args`              | Parse `#[wavedb(...)]` attribute arguments.                     |
| `struct_hash`       | Compute the `STRUCT_HASH: u64` const from name/shape/fields.    |
| `descriptor`        | Emit `ObjectDescriptor` (field offsets, heapable flags, names). |
| `wire_derive`       | `WaveWire` — the no-serde, no-`repr(C)` `Wire` impl.            |
| `generated`         | Auto-emit the per-NonUnique `Pivot` + `BpTree` types.           |
| `crud`              | Generated accessors / CRUD glue.                                |
| `declare`           | `declare_objects!` registry codegen.                            |
| `codegen` / `utils` | Shared emit helpers.                                            |

---

## What `#[wavedb]` does at compile time

1. **Computes `STRUCT_HASH: u64`.** A `const` hash of
   `STRUCT_NAME + SHAPE + each PROPERTY_NAME + each PROPERTY_TYPE`. Because field
   names and types are folded in, **any schema change changes the hash** — a
   changed struct is simply a different type. There is **no `version =` attribute
   and no version-from-type-name rule** anymore; identity is the hash.
2. **Implements `Id` + `Metadata` accessors.** `.tenant_id()`, `.key()`,
   `.created_at()`, `.struct_hash()`, plus full `Metadata` getters/setters — no
   call-site boilerplate.
3. **Emits the `Wire` impl.** Byte layout is defined explicitly by `WaveWire`
   (see `docs/wire_format.md`) — no `serde`, no `repr(C)`. The macro decides
   every field's stack/heap placement, so layout never depends on the Rust
   compiler's struct representation.
4. **Generates the collection machinery for `NonUnique`** — a `Pivot` type and a
   `BpTree` type (below).
5. **Wires up the schema-evolution hooks** — the optional `first_try` /
   `fallback_not_found` functions; see
   [`wavedb-core`](../wavedb-core/README.md#schema-evolution--lookup-hooks).

```rust
#[wavedb]                       // Unique by default
pub struct AboutUser {
    pub name: String,
    pub surname: String,
    pub phone: String,
    pub address: String,
}
```

All `#[wavedb]` structs serialize through WaveDB's own `Wire` format; the hook
fns are generic over `Db` so the macro's `__WaveDbDb` parameter resolves at the
call site.

---

## Data shapes

| Marker (none = Unique) | Cardinality per tenant                       | ID `KEY`               |
| ---------------------- | -------------------------------------------- | ---------------------- |
| _(default)_ **Unique** | Exactly one live record per tenant           | `STRUCT_HASH` (anchor) |
| `NonUnique`            | Many per tenant; may nest in other NonUnique | `CREATED_AT`           |

```rust
#[wavedb(NonUnique)]
pub struct Order {
    pub amount: u64,
    pub note: String,
}
```

`NestedNonUnique` is **not a separate marker** — you nest by holding a `PivotId`
of another NonUnique collection in a field. The collection is reached by
`get()`-ing that pivot (see below).

---

## Generated collection types (`Pivot` + `BpTree`)

`#[wavedb(NonUnique)]` auto-generates two helper types per family. They carry no
business data — they are pure addressing — so applications rarely name them
directly; they reference a collection through its `PivotId`.

```rust
// Auto-generated. The handle for one NonUnique collection.
pub struct Pivot {
    pub counter: u64,       // number of live elements
    pub current: BpTreeId,  // u128 → B+tree of living records
    pub dead:    BpTreeId,  // u128 → B+tree of deleted records
}

// Auto-generated. A B+tree keyed by CREATED_AT, holding NonUnique record
// addresses (not their bytes). One instance indexes living data, one deleted.
pub struct BpTree { /* node entries → Id */ }
```

Referencing a collection from another object:

```rust
#[wavedb]
pub struct UserInterestedFruits {
    // a handle into a NonUnique collection of Fruit, reached via its Pivot
    pub list_of_fruits: <Fruit as WaveDbStruct>::PivotId,
}
```

> The generated `Pivot`/`BpTree` types share a name and field shape across
> families, so their `STRUCT_HASH` may collide. That is harmless — they are only
> ever addressed within their own tenant/collection context. Both are
> timestamp-keyed, so an 8-bit `STRUCT_HASH` truncation rides in the `Id`'s
> `SALT` — `salt7 ‖ trunc8(STRUCT_HASH)`, the same packing every timestamp-keyed
> shape uses (see
> [`wavedb-core`](../wavedb-core/README.md#the-salt-field-15-bits)).

---

## Validation & preprocessing hooks

Two attributes, run identically native and wasm32, dispatched through a static
monomorphised fn table (no `dyn`, no async in the WASM binary):

```rust
#[wavedb(NonUnique, validate = validate_payment, preprocess = preprocess_payment)]
pub struct Payment { pub amount_cents: u64, pub currency: String }
```

| Hook         | Client (pre-send)                | Node (pre-commit)                         | Purpose                       |
| ------------ | -------------------------------- | ----------------------------------------- | ----------------------------- |
| `validate`   | ✓ — typed error, zero round-trip | ✓ — the security boundary                 | invariant checks              |
| `preprocess` | ✗                                | ✓ — re-encoded result is what's committed | normalisation, derived fields |

The node runs `validate` **before** `preprocess`. Hooks are synchronous and
pure; hook-less types skip decode entirely via compile-time `HAS_VALIDATE` /
`HAS_PREPROCESS` consts. Node-side enforcement (the gate order) is documented in
[`wavedb-quick-node`](../wavedb-quick-node/README.md#node-side-enforcement).

> Richer **server-side functions** (async, with DB access — the "backend" half of
> full-stack) are planned as a separate attribute family; see the roadmap.

---

## `declare_objects!` — the registry

```rust
declare_objects! { pub mod app_objects { payments: [Payment], orders: [Order], … } }
```

Emits per-family modules, a `find(struct_hash)` const-compare chain (no `dyn`,
compile-time duplicate-hash check), per-struct `DESCRIPTOR: &'static
ObjectDescriptor` (field offsets, heapable flags, heap-prop name list, shape),
and `REGISTRY: &'static ObjectRegistry`. `WaveDbStruct::STRUCT_HASH` is the
lookup key. Attaching the registry is what turns a generic node into _your_
backend, and it is the same `REGISTRY` compiled into every client and node so the
schema crate is the single source of truth.
