# RFC 0015 — The `#[wavedb]` declarative macro

- **Status:** Implemented
- **Crates:** `wavedb-macros`
- **Code:** `crates/wavedb-macros/` (`wavedb_attr.rs`, `generated.rs`,
  `secondaries.rs`, `natural_key.rs`)

## Summary

`#[wavedb]` is the surface a developer declares an object with. From a plain
struct it emits everything the engine needs — the type identity, the codec, the
per-type generated helpers, the per-operation execution steps, and the storage
slots — so that a declared type is *immediately* a full participant with no
hand-written engine glue.

## Motivation

The [no-`dyn`, `match`-on-`STRUCT_HASH`](0002-architectural-hard-rules.md) model
means every type's operations must exist as **concrete, monomorphized code** at
the item itself. That is a lot of boilerplate (encode/decode, id minting,
per-command steps, index hooks) — mechanical, error-prone, and identity-load-
bearing. A proc-macro is the only place it can be generated *and* kept in lockstep
with the `STRUCT_HASH` that names it.

## Design — what the macro emits

- **`STRUCT_HASH`** — the `const` SeaHash over name+shape+fields
  ([RFC 0004](0004-struct-hash-and-schema-evolution.md)); any schema change =
  new type.
- **`WaveWire`** encode/decode ([RFC 0003](0003-wavewire-wire-format.md)).
- **Generated `{Name}Pivot` / `{Name}PivotId`** for NonUnique types
  ([RFC 0011](0011-bptree-index-and-collections.md)).
- **Per-command execution steps** — `__wavedb_{get,save,insert,update,remove,all}`,
  the concrete arms a registry `match` dispatches to
  ([RFC 0017](0017-exposure-registry-and-side-features.md)). These drive
  `Collection::at(...)` directly, so the wire ops never depend on the generated
  wrapper shape.
- **Per-type `static StructStorage` slots** (native only; the wasm expansion
  omits them) — the compile-time per-type state the engine keys on
  ([RFC 0018](0018-storage-engine.md)).
- **Secondary-index hooks** from `#[wavedb::pivot(field)]` — a `by_field` lookup
  and the index writes ([RFC 0011](0011-bptree-index-and-collections.md)).
- **Natural-key anchors** from `#[wavedb::key(f1, …)]` — `natural_key()` and the
  `#key` STRUCT_HASH fold ([RFC 0012](0012-natural-keys.md)).

## Design — shape markers

The macro emits marker traits (`UniqueStruct` / `NonUniqueStruct`,
`PivotHandle`) that gate the two shapes at the type level, so the client and
node surfaces resolve the right operation set at compile time without runtime
tags.

## The typed surface

Generated methods spell `T::get(&db)` / `v.save(&db)` / `T::collection(pivot)`
against the `DbHandle` seam ([RFC 0024](0024-client-db-and-cache.md)), so one
body text runs against a client `Db`, a node `ServerDb`, or a bare
`LocalHandle`.

## Consequences

- **File budget** ([RFC 0002 §6](0002-architectural-hard-rules.md)) forced the
  macro crate to split by concern (`wavedb_attr` / `generated` / `secondaries` /
  `natural_key`), each with one responsibility.
- **Per-struct wasm cost was measured** (the M1 risk): ~23 B raw / 18 B gzip per
  exposed struct for the pure `match` arm, ~204 B raw with a novel decode shape —
  the registry scales, no sum type needed ([RFC 0025](0025-wasm-indexeddb-target.md)).
