# RFC 0040 — Schema migration across node/client version skew

- **Status:** Deprecated (dropped 2026-07-28 — never shipped)
- **Supersedes:** [RFC 0014](0014-schema-evolution-hooks-DEPRECATED.md)
- **Superseded by:** nothing. Schema migration is **the developer's**, entirely;
  the engine keeps only the identity-collision guard described below.
- **Crates (built, then removed):** `wavedb-core`, `wavedb-macros`
- **Related:** [RFC 0004](0004-struct-hash-and-schema-evolution.md) (the identity
  rule that stands), [RFC 0017](0017-exposure-registry-and-side-features.md)
  (where the surviving guard lives)

## Decision

**WaveDB does not migrate schemas.** A changed struct is a new type
([RFC 0004](0004-struct-hash-and-schema-evolution.md)); old and new bytes
coexist, and moving data between them — if an application wants it moved at all —
is application work, written against the ordinary read/write surface like any
other domain logic. The engine offers no version chain, no upgrade/downgrade
converters, no lazy materialisation, and no version-aware read walk.

What the engine *does* keep is one guard, at the registry, in **compile time**:

> `expose_server!` / `expose_client!` emit, per declared pair, a const-evaluated
> check that the two entries are distinct on the full 64-bit `STRUCT_HASH` (a
> **hard error** — two entries sharing it are one identity on the wire, and one
> arm would shadow the other in every dispatch `match`) and on the 15-bit
> `type_salt` (a **warning** — reads stay correct because the full head is
> verified, but the pair shares archive slots and loses its separation in the
> browser's flat keyspace).

Both diagnostics are spanned at the offending entry in the exposure list, so the
compiler underlines the line that must change. `fn` entries join the 64-bit check
(functions and structs share one dispatch hash space) and sit out the salt check
(functions are never stored). Implemented in
`wavedb-macros/src/expose_collision.rs` against
`wavedb_core::expose::SaltGuard`; proven by the `compile_fail` doctests in
`examples/schema-smoke` — a warning is not observable from a test body, so the
proof promotes it with `#![deny(deprecated)]`.

## Why the mechanism was dropped

The design below was worked out in full and partly built: a `Versioned` chain
through an associated `Prev` type, a monomorphized `resolve` walk over derived
`SALT` slots, `#[wavedb(prev = …)]` wiring, and a version-aware `Unique` read in
the node dispatch. Reviewing it against the rest of the system surfaced a cost
the feature could not pay for:

- **A large, load-bearing mechanism for a rare event.** Every read path (node
  dispatch, `LocalHandle`, `ServerDb`, the client cache) would have to become
  version-aware to be consistent — otherwise migration applies in some execution
  contexts and silently not in others.
- **The write direction was the hard half.** A peer at an older shape writes
  through an upgrade converter that cannot know the fields it never saw; serving
  that peer needs the current shape downgraded on the way out and re-upgraded on
  the way in. Every seam is a chance to drop a field silently — exactly the class
  of bug the "a changed struct is a new type" rule exists to avoid.
- **Lazy materialisation makes reads write.** Writing the upgraded record at the
  current version's slot collides with the `Write::Expect` guard, permission
  gates, read-only cache paths, and wasm. Leaving it out means the walk pays its
  cost on every read, forever.
- **The NonUnique half was unresolved.** Quick-upgrading a `Pivot` by copying its
  root ids makes two versions share tree nodes, so the older version stops being
  the frozen snapshot [RFC 0009](0009-anchors-succession-and-history.md)
  promises; secondary indexes copied across a shape change carry wrong keys; and
  preserving a record's original instant on migration can author *below* a live
  catch-up cursor, hiding the migrated record from every watcher.

Against a stance of **single-tenant, online-first, small offline window**, a
developer-written migration — read the old type, write the new one, drop the old
declaration when done — is simpler to reason about and impossible to get subtly
wrong in the engine's name.

## What the developer does instead

Nothing is prescribed and nothing is generated. The pattern falls out of
[RFC 0004](0004-struct-hash-and-schema-evolution.md):

- Declare the new shape as its own type. The old one keeps its own
  `STRUCT_HASH`, its own storage slot, and its own data, untouched.
- Keep both exposed for as long as both must be readable; the collision guard
  covers that pair like any other two types.
- Move the data with ordinary application code (a `#[server]` function, a
  one-shot admin path, or on demand inside the app's own read), then retire the
  old declaration.
- Bytes are never destroyed by the engine, so an interrupted migration loses
  nothing — the old records stay exactly where they were.

## The dropped design (record only)

Kept so the idea is not re-invented from scratch. Three parts:

1. **Numbered types + a re-export alias** — `Task1`, `Task2`, …, with
   `pub type Task = TaskN` naming the current shape. Because `#[wavedb]` folds
   the *written* field-type text (`normalise_type` is
   `quote!(#ty).to_string()…`) and a proc macro cannot resolve an alias, a holder
   that writes `tasks: <Task as WaveDbStruct>::PivotId` folds the text `"Task"` —
   so flipping the alias evolves a member **without changing any holder's
   `STRUCT_HASH`**, killing the cascade up to the `Unique` root.
2. **Version addressing by `SALT` derivation** — a version's home is the base id
   with its 15-bit `SALT` replaced by `type_salt(TaskN::STRUCT_HASH)`, recomputed
   on every read rather than stored, so the holder is never rewritten and
   unbounded versions cost it nothing.
3. **A compile-time chain, monomorphized** — `trait Versioned { type Prev; const
   IS_FIRST: bool; }`, the first version terminating at `Prev = Self`; a generic
   `resolve<T>` probing `T`'s slot, recursing to `T::Prev` on a miss and applying
   a developer-written `UpgradeFrom` on the unwind. The generated code never
   names an intermediate version (it traverses by projection), so no `dyn` table
   and no runtime registry — the walk monomorphizes into concrete arms. A head
   mismatch at a probed slot reads as a `SALT` collision and is skipped, making
   the walk collision-safe by construction. `DowngradeFrom` was the inverse, for
   serving a peer that only knows an older shape.

One hazard is worth restating, because it applies to any future attempt: once
data has moved `Task1 → Task2 → Task3`, deleting `Task2` from the codebase makes
`type_salt(Task2)` uncomputable, orphaning anything still lagging at or below it
— unreachable and unreclaimable.

## Alternatives

- **A global migration walk** — the stop-the-world sweep
  [RFC 0004](0004-struct-hash-and-schema-evolution.md) exists to avoid. Still
  rejected; a developer-written migration is the same work, but explicit, scoped,
  and interruptible.
- **Type-erasing the pivot reference** — considered while the mechanism was
  alive; rejected because it hides the member's type at the call site.
- **Keeping the version chain for `Unique` only** — rejected: a half-covered
  mechanism is worse than none, because it teaches a guarantee the NonUnique path
  does not honour.
