# RFC 0004 — STRUCT_HASH identity & schema evolution

- **Status:** Implemented
- **Crates:** `wavedb-core`, `wavedb-macros`
- **Code:** `#[wavedb]` expansion (`crates/wavedb-macros/`); `seahash` pinned
  `=4.1.0` in the workspace `Cargo.toml`
- **Depends on:** [RFC 0002 §4](0002-architectural-hard-rules.md) (pinned
  identity deps)

## Summary

Every declared struct gets a `STRUCT_HASH: u64` computed **at compile time** by
the `#[wavedb]` macro as a `const` SeaHash of the type's *shape*:

```
hash( STRUCT_NAME + SHAPE(Unique | NonUnique) + each FIELD_NAME + each FIELD_TYPE )
```

Because names and types fold in, **any schema change produces a new
`STRUCT_HASH`** — a changed struct is simply a different type. There is no
separate "struct id + numeric version"; the hash subsumes both.

## Motivation

Two problems collapse into one mechanism:

- **Type routing.** Storage directories, wire frames, and dispatch all need to
  name a type by a fixed handle. A compile-time `u64` is that handle, and it is
  the value every `match` in the system switches on ([RFC 0002 §1](0002-architectural-hard-rules.md)).
- **Schema evolution.** Traditional migrations rewrite data to a new version
  number. WaveDB instead makes the *changed* type a *different* type: old bytes
  keep decoding under the old hash, new writes use the new hash, and clients,
  servers, and nodes can run different builds simultaneously — no global
  migration step, no backup-and-restore.

## Design

- **`const`, SeaHash, pinned.** The hash runs in `const` context so it composes
  into other crates' consts (e.g. function identity,
  [RFC 0016](0016-server-functions.md)). SeaHash is portable across
  arch/endianness for a fixed seed, so a stored hash means the same thing on
  every target. The version is pinned exactly (`=4.1.0`) because the output is
  persisted — a bump would silently invalidate every stored type tag.
- **One hash space for structs *and* functions.** A `#[server]` fn gets its own
  `STRUCT_HASH` in the same 64-bit space; at the frame level a function call is
  indistinguishable from an object op ([RFC 0020](0020-net-transport-dumb-tunnel.md)).
- **Synthetic shape entries.** Declarations that change identity but are not
  plain fields fold in as synthetic entries — e.g. a `#[wavedb::key(...)]`
  natural key adds a `#key` entry ([RFC 0012](0012-natural-keys.md)), so
  changing the key is a schema change.
- **Generated per-type helpers collide harmlessly.** The macro also generates a
  `Pivot`/`PivotId` per NonUnique type; two generated types with the same name
  and shape may share a hash, which is harmless because they are only ever
  addressed within their own tenant/collection context.

## Schema-evolution *bridging* is a separate concern

The hash makes old and new coexist; *reconciling* them (reading V1 bytes into a
V2 shape) is done by application hooks, not an engine walk — see
[RFC 0014](0014-schema-evolution-hooks-PLANNED.md) (`first_try` / `fallback_not_found`).

## Alternatives & prior art

- **Struct id + numeric version field in the ID** (the pre-rebuild model):
  dropped. It forced a migration chain and an auth-of-version story; the shape
  hash subsumes both and removes two fields from the `Id`
  ([RFC 0005](0005-composite-ids-and-bit-budgets.md)).
