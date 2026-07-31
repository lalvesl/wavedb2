# RFC 0017 — The exposure registry & schema side-features

- **Status:** Implemented
- **Crates:** `wavedb-macros`
- **Code:** `expose.rs`, `expose_parse.rs`; the `server-side` / `client-side`
  cargo features

## Summary

`expose_server!` / `expose_client!` are the **declared allowlist registry**: one
`match` per operation over exactly the listed items. Nothing is wire-reachable
until it is exposed. Unlisted, excluded, or wrong-shape commands all refuse as a
**uniform `UnknownStructHash`** — deliberately indistinguishable, for security.
A companion feature system (`server-side` / `client-side`) guarantees a deployed
binary never even *compiles in* the other side's logic.

## Motivation

Two security properties fall out of making the registry an explicit list rather
than an auto-scan:

1. **Exposure is a surface.** A type can exist in storage and inside `#[server]`
   bodies yet be *unnameable* on the wire; any generated op can be excluded or
   swapped for a hardened reimplementation. The allowlist is the thing a reviewer
   reads to know the attack surface.
2. **No information leak on refusal.** An unlisted type, an excluded op, and a
   type that never existed must all look *identical* to a probing client — so a
   caller cannot map the schema by probing.

And, separately: server-only logic (validation, secrets, business rules) must
**not ship in the client binary** — a stronger guarantee than trusting LTO to
strip it.

## Design — the registry

- **One `match` per operation** over the listed items — concrete monomorphized
  arms ([RFC 0002 §1](0002-architectural-hard-rules.md)); no build-time scanner,
  no `build.rs`, no `dyn`, no sum type.
- **Uniform refusal.** Every non-match — unlisted hash, excluded op, wrong shape
  — returns `UnknownStructHash`. Functions and objects share the hash space, so
  a fn call and an object op are indistinguishable at the frame level
  ([RFC 0016](0016-server-functions.md)).
- **`store`-only entries.** `store Path` contributes a type's `storage_entries()`
  to the emitted `StorageRegistry` and *nothing else* (`knows = false`, wire
  refusal unchanged) — storage-only types. `expose_client!` rejects `store`
  entries (no engine client-side). `expose_server!`/`expose_client!` also emit
  the `StorageRegistry`, so `.registry(REGISTRY)` alone opens the `PageStore`
  ([RFC 0023](0023-quick-node-and-gates.md), [RFC 0024](0024-client-db-and-cache.md)).

## Design — side features (the no-leak contract)

A crate expanding these macros declares two cargo features named **exactly**
`server-side` and `client-side`. Emission gates on them:

- `#[server]` bodies + `__wavedb_dispatch` + all of `expose_server!` → under
  `server-side`;
- client stubs + `expose_client!` → under `client-side`;
- the fn-type / `STRUCT_HASH` and all `#[wavedb]` struct machinery → under
  **both** (the schema *is* the protocol).

A deployed binary depends on the schema `default-features = false` + exactly its
side, so the other side is **never compiled in** — the guarantee is the `cfg`,
not dead-code stripping. Hand-written server-only helpers carry
`#[cfg(feature = "server-side")]` themselves; `expose_server!` `compile_error!`s
a `wasm32 + server-side` build. Defaults keep **both on** so a schema crate's own
tests drive the full loop; deployments opt *down*.

## Deferred

- **`update_call` exposure kind** — a declared entry exposing a mutating-call
  shape distinct from the plain object ops. Not built; recorded so the reserved
  idea is not re-invented.

## Alternatives

- **Auto-scanning all `#[wavedb]` types into the registry** — rejected: it makes
  exposure implicit, defeats the allowlist-as-surface property, and offers no
  place to exclude or harden an op.
- **Relying on LTO/DCE to drop server logic** — rejected as the guarantee; the
  `cfg` split makes it a compile-time fact instead of an optimiser's discretion.
