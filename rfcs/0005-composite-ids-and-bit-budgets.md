# RFC 0005 — Composite IDs and bit budgets

- **Status:** Implemented
- **Crates:** `wavedb-core`
- **Code:** `Id`, `LocalId`, `U48` (`crates/wavedb-core/src/`); minting in
  `core::mint` + `wavedb_platform::time::key_nanos`

## Summary

Every record has a **128-bit composite `Id`**. The most-significant field is the
`KEY`, so a numeric sort of the `u128` is a sort by key — chronological for
timestamp-keyed records, ideal for the `BpTree`.

```
[ KEY (u64) | TENANT (u48) | FLAG (1) | SALT (15) ]
   MSB ─────────────────────────────────────── LSB
```

| Field | Width | Meaning |
|-------|-------|---------|
| `KEY` | u64 | A `STRUCT_HASH` (Unique) **or** a `CREATED_AT` instant — disambiguated by `FLAG`. |
| `TENANT` | u48 | Owning tenant. For B2C, the user id. |
| `FLAG` | 1 | `1` ⇒ `KEY` is a struct-hash (Unique anchor); `0` ⇒ `KEY` is an instant. |
| `SALT` | 15 | Collision breaker within one `(KEY, TENANT)`. |

## Motivation

The ID must simultaneously: sort chronologically for the index, name the
tenant structurally (so it never rides in a query, [RFC 0007](0007-tenancy-and-data-ownership.md)),
distinguish the Unique anchor from timestamp-keyed shapes, and stay collision
free even on a coarse clock. Packing those into 128 bits with the key on top
gives all of it without a secondary sort structure.

## Design

- **`SALT` only breaks collisions.** Unique = `0`; every timestamp-keyed shape
  (NonUnique / BpTree / Pivot) uses it as a discriminator. It carries **no**
  struct-hash truncation — the type is known from the per-`STRUCT_HASH` storage
  directory and the wire envelope, and the 48-bit `TENANT` separates tenants.
  (Exception: an *archive* slot derives `SALT = trunc(STRUCT_HASH)` so types
  can't collide in a flat keyspace — see [RFC 0009](0009-anchors-succession-and-history.md).)
- **`LocalId` = 80 bits** (`KEY u64 · FLAG 1 · SALT 15`): an `Id` with the
  `TENANT` stripped, for BpTree-internal pointers where the tenant is known from
  tree scope.
- **`U48`** is the tenant/user scalar type; **block descriptor** is a separate
  64-bit pack (`start u40 · count u20 · occupation u4`) reused for pages *and*
  dictionaries in storage ([RFC 0018](0018-storage-engine.md)).
- **Minting bottoms out in `key_nanos()`.** All instant minting goes through
  `wavedb_platform::time::key_nanos` = real milliseconds × 1e6 + a process-wide
  atomic counter in the dead sub-ms digits — the *same formula both targets*, so
  even the browser's millisecond clock can't collide. Collection minting layers
  a **monotone floor** on top (`core::mint::mint_instant`) so a rewound clock
  can never write under a cursor a client already passed
  ([RFC 0011](0011-bptree-index-and-collections.md)).

## Notes & history

- **No `STRUCT_ID`, no schema-version field.** Both were removed when
  `STRUCT_HASH` ([RFC 0004](0004-struct-hash-and-schema-evolution.md)) subsumed
  type identity. No reserved bits remain.
- **`CREATED_AT` ordering is best-effort.** Uniqueness within `(KEY, TENANT)`
  comes from the salt/counter, not clock monotonicity — fine for the index,
  never relied on as a strict total order under skew.
- **Deferred — per-user-session `SALT` masking.** Showing the same record's
  15-bit salt differently per user session is a possible privacy refinement;
  not built, noted so the idea survives.
