# RFC 0040 — Schema migration across node/client version skew

- **Status:** Planned
- **Supersedes:** [RFC 0014](0014-schema-evolution-hooks-DEPRECATED.md)
- **Crates:** `wavedb-core`, `wavedb-macros`, `wavedb` (client), `wavedb-quick-node`
- **Depends on:** [RFC 0004](0004-struct-hash-and-schema-evolution.md),
  [RFC 0005](0005-composite-ids-and-bit-budgets.md),
  [RFC 0009](0009-anchors-succession-and-history.md),
  [RFC 0011](0011-bptree-index-and-collections.md),
  [RFC 0017](0017-exposure-registry-and-side-features.md)
- **Code (target):** `wavedb_core::mint` (`type_salt`, `archive_id`),
  `wavedb_core::local_id`, `wavedb_core::record` (head-verify),
  `wavedb-macros` (`normalise_type`, `pivot_identity`, the `expose_*` registry),
  `wavedb` typed read surface.

## Summary

`STRUCT_HASH` makes a changed struct a **new type**, so old and new bytes coexist
with no migration ([RFC 0004](0004-struct-hash-and-schema-evolution.md)). This RFC
turns that identity rule into a **transparent, forward-migrating story that
survives a node running ahead of its clients**, and does it without a global
upgrade walk, without touching the holder of a nested collection, and without ever
destroying the old bytes.

Three moving parts:

1. **Numbered types + a re-export alias.** Each schema type is declared as a
   numbered concrete struct — `Task1`, `Task2`, … — with `pub type Task = TaskN;`
   naming the current shape. Every relation, call site, and nested-pivot field
   references the **alias** `Task`. Because the `#[wavedb]` macro folds the
   *written* field-type text (not the resolved type), flipping the alias evolves a
   member **without changing any holder's `STRUCT_HASH`** — the cascade to a
   `Unique` root is gone.
2. **Version addressing by SALT derivation.** A version's on-disk home is a
   *computed* address — the same 15-bit `SALT` trick archives already use — reached
   by a **downgrade-walk** and then lazily materialised in place. Nothing is stored
   to point at it; the holder is never rewritten.
3. **Two developer-written, engine-invoked converters.** `UpgradeFrom<Old>` bridges
   an older on-disk record up to the current shape on read; `DowngradeFrom<New>`
   serves an older peer the current data in its shape. Both run automatically —
   business code never mentions "version".

## Motivation

Coexistence-by-hash ([RFC 0004](0004-struct-hash-and-schema-evolution.md)) still
owes an answer to *"the code wants `Task2` and only a `Task1` record is on disk."*
The classic answer — a global migration walk — is precisely the friction that
design removes. Three forces shape the alternative:

- **The node is always at or ahead of every client, never behind** (deploy the node
  first). So skew is real, one-directional, and must be handled *live* — a newer
  node reading its own older data, and serving a client that only knows an older
  shape.
- **Nested collections must not cascade.** A holder stores a member collection's
  `PivotId`; if evolving the member changed the holder's identity, the change would
  ripple up every ancestor to the `Unique` root ([RFC 0011](0011-bptree-index-and-collections.md)).
  That is unacceptable for a leaf-type edit.
- **History is sacred.** Bytes are never destroyed ([RFC 0009](0009-anchors-succession-and-history.md));
  migrating forward must leave the old version navigable for history and backups,
  not overwrite it.

## Design

### 1. Numbered types and the alias (the developer surface)

A type is born numbered, with an alias:

```rust
#[wavedb(NonUnique)]
struct Task1 { /* … */ }
pub type Task = Task1;              // relations & call sites use `Task`, never `Task1`
```

Evolving it is copy-paste-forward, all developer-driven and explicit:

```rust
#[wavedb(NonUnique)]
struct Task2 { /* new shape */ }
impl UpgradeFrom<Task1> for Task2 { /* the conversion, written by hand */ }
pub type Task = Task2;             // one line flips the current shape
// registry now lists BOTH Task1 and Task2 (a superset — the node decodes both)
```

**Why this kills the cascade.** The `#[wavedb]` macro hashes the *written token
text* of each field type — `normalise_type` is `quote!(#ty).to_string()...` — and
never resolves aliases (a proc-macro cannot). A holder that writes
`tasks: <Task as WaveDbStruct>::PivotId` folds the text `"Task"`. Flipping
`pub type Task = Task1` → `Task2` changes the *resolved* type at compile time
(`Task2PivotId`) but **not** the folded text, so `Project::STRUCT_HASH` is
invariant across `Task`'s evolution. The identity graph (holders) and the evolution
graph (members) are decoupled by the alias alone — no type erasure, and the
member's type stays visible at the call site.

**The one discipline:** relations reference the alias `Task`, never the concrete
`TaskN`. A holder that hard-codes `Task1` is deliberately pinned to v1 (occasionally
useful) — worth a lint.

### 2. Version addressing by SALT derivation (no stored pointer, no extra IOp)

The 15-bit `SALT` of an id is already *"what makes archive addresses derivable
without storing them"* (`mint::type_salt`, and `archive_id` for a superseded
record's slot). This RFC reuses that mechanism on the **version** axis:

- A version's pivot/record home = the base id with its `SALT` **replaced** by
  `type_salt(TaskN…::STRUCT_HASH)` (the low 15 bits of the version's hash).
  `LocalId` grows a `with_salt(...)` companion to its existing `salt()`.
- The **holder is never modified.** Its stored `PivotId` carries whatever `SALT` it
  was minted at; the current version's address is *recomputed* on every read by
  swapping in `type_salt(current)`. The stored `SALT` is only a base; the KEY /
  TENANT / FLAG bits are the stable part. This is why **unbounded versions cost the
  holder nothing.**

### 3. The read walk: `prefer_current` → downgrade → `upgrade_on_miss`

```
read at current version V_cur:
  addr = base.with_salt(type_salt(V_cur))
  hit  → done.                                   ← prefer_current, O(1) steady state
  miss → for V in (V_cur-1, V_cur-2, … known to the CODE):
           read base.with_salt(type_salt(V))
           hit → upgrade_on_miss:
                   UpgradeFrom V → … → V_cur       (chained, dev-written)
                   materialise V_cur at its slot   (lazy, ADDITIVE — old slot kept)
                   return upgraded value           (next read hits directly)
         exhausted, nothing on disk → CRITICAL FAILURE
```

- The walk is **structural, not `got`-dispatched.** It descends the version chain
  (§3.1) and probes each version's *statically-known* `SALT` slot; the hit level is
  decoded as its own concrete type. The head-verify's `Error::UnknownStructHash(got)`
  (`record::decode_envelope` / `split_record`) is only the **miss/collision signal**:
  a 15-bit `SALT` collision that lands on a foreign slot fails the full 64-bit head
  check → treated as a miss, walk continues → **reads are collision-safe by
  construction.**
- **Cost.** Pre-migration: up to *N* disk reads, where *N* is the number of
  versions the **current code** declares — once per collection/record. Post-
  migration: O(1); the current-version slot hits first. The permanent overhead is
  the CPU of `hash & 0x7FFF`, not an extra disk hop — there is no stored
  indirection to chase.

### 3.1 Rust realization — a compile-time chain, monomorphized

The walk must reach `TaskN-1` (a type that may live in another module) **without the
generated code ever naming it** — and without a `dyn` table or runtime registry
([RFC 0002](0002-architectural-hard-rules.md)). The chain is a compile-time linked
list through an **associated type**, and the walk is a **generic recursion** the
compiler monomorphizes into concrete arms:

```rust
// core traits. `#[wavedb(prev = Task2)]` on Task3 emits the `Versioned` impl;
// the FIRST version gets a macro-emitted identity terminator (`type Prev = Self`).
trait Versioned: WaveWire + Sized {
    type Prev: Versioned;          // predecessor; first version → Self
    const IS_FIRST: bool;
    const STRUCT_HASH: u64;
}
trait UpgradeFrom: Versioned { fn upgrade_from(prev: Self::Prev) -> Self; }     // dev-written, pairwise
trait DowngradeFrom: Versioned { fn downgrade_from(cur: Self) -> Self::Prev; }  // dev-written, pairwise

fn resolve<T: UpgradeFrom>(store: &S, base: LocalId) -> Result<Option<T>> {
    let addr = base.with_salt(type_salt(T::STRUCT_HASH));
    match store.get_of(T::STRUCT_HASH, addr)? {
        Some(bytes) => decode::<T>(bytes).map(Some),      // hit at this version
        None if T::IS_FIRST => Ok(None),                  // genuine miss (or CRITICAL if a version was deleted)
        None => match resolve::<T::Prev>(store, base)? {  // recurse DOWN via Prev
            Some(prev) => Ok(Some(T::upgrade_from(prev))), // UpgradeFrom on the way UP
            None => Ok(None),
        },
    }
}
// entry is ALWAYS the current alias:  Task = Task3  →  resolve::<Task3>(store, base)
```

Why this answers "`TaskN-1` is in another module":

- **The generated code never names an intermediate.** `resolve` traverses the middle
  by `T::Prev` — a projection the trait system resolves — so its source mentions only
  the type parameter. The *only* place `Task2` is spelled is the developer's
  `#[wavedb(prev = Task2)]` (or their `UpgradeFrom` impl), a normal Rust path resolved
  the normal way; cross-module is a non-issue there.
- **The base case is a runtime test, not a type-level `From == To`** (which Rust
  cannot express without specialization): each level probes its own slot and decodes
  as its own concrete type; upgrades apply on the unwind.
- **`resolve::<Task3>` monomorphizes to `{Task3, Task2, Task1}`** — concrete static
  calls, no `dyn`, no fn-pointer table, no runtime registration. `Task1::Prev = Self`
  makes the base frame's `resolve::<Task1::Prev>` a guarded, dead self-reference (no
  infinite monomorphization).

The inverse direction reuses the **same** `Prev` chain: an older client's write is
lifted to the current anchor by descending until `T::STRUCT_HASH` matches the
written hash, decoding there, then unwinding through `upgrade_from`; serving an older
reader descends from the current alias applying `downgrade_from` until it reaches the
version the registry arm was generated for. No `Next` chain is ever needed.

### 4. Serving older peers: `DowngradeFrom`, automatic

A node ahead of a client that only knows `Task1` reads its current (`Task2`) data
and runs `DowngradeFrom` `Task2 → Task1` on the fly. This is **distinct** from the
frozen v1 snapshot on disk (§5): the snapshot is *history as it was*; a downgrade
serves *current data in an old shape*. Both converters are pairwise, written by the
developer, and invoked by the engine — never surfaced to caller code.

### 5. Additive migration, immutable history

Migration writes the new version at *its* `SALT` slot; the old slot lives at a
*different* address and is **never overwritten**. So the old-version pivot and its
trees remain a frozen, navigable snapshot for history and backups — the collection-
level extension of [RFC 0009](0009-anchors-succession-and-history.md)'s "addresses
are computed, links are instants, no archive is repointed."

### 6. Collision safety — a generated test in `expose_*`

`SALT` is 15 bits (32 768 values); a full `STRUCT_HASH` is 64. Two coexisting
versions (or any two exposed types) that collide on `SALT` share a slot; a full-u64
collision makes two types indistinguishable at dispatch. The exposure registry
([RFC 0017](0017-exposure-registry-and-side-features.md)) is the one place that
already enumerates *every* type, so it is where the guard belongs:

> **`expose_server!` / `expose_client!` emit a `#[cfg(test)] #[test]` that asserts,
> across every listed type _and its generated `Pivot`_, that both the 64-bit
> `STRUCT_HASH` and the 15-bit `type_salt` are pairwise unique — failing the test
> suite (naming the offenders) on any collision.**

Reads stay safe regardless (§3, head-verify); the test makes a collision **loud at
dev time** instead of a silent stranded-write risk, and it catches the SALT clash
between `Task1`/`Task2`/`Task3` that must coexist during a migration.

### 7. Developer discipline & warnings (advisory, never blocking)

The disk does **not** know how many versions exist — that lives only in the code
(how many `TaskN` are declared). Two hazards follow; both are the developer's to
manage, and the tooling should *warn*, not enforce:

1. **Version-skew hygiene.** When a downgrade-walk fires, on-disk data lags the
   code and the disk doesn't record that a holder now relates to the newer member.
   Advisory: bump the holder to a new version if you want the disk/backup
   self-describing. The mechanism works forever without it.
2. **Stranded intermediate-version data.** Forward migration leaves each superseded
   slot on disk, unvisited by the live read path once a newer slot exists. An
   **intermediate** version is the trap: once data has moved `Task1 → Task2 →
   Task3`, the `Task2` slot is never revisited, and if `Task2` is later **deleted
   from the codebase**, `type_salt(Task2…)` can no longer be computed — that data is
   **orphaned forever** (unreachable, un-reclaimable). Worse, any record still
   lagging at or below a deleted intermediate can no longer be chained forward
   (`UpgradeFrom` is pairwise), so it becomes silently inaccessible — the
   *exhausted-walk critical failure* of §3. **Never delete a version until all data
   has migrated off it**, and prefer draining a version before retiring the one
   below it.

### Naming

| Role | Name |
|------|------|
| read pre-empt: try the current version first | `prefer_current` (was `first_try`) |
| read post-miss: walk down, `UpgradeFrom`, materialise | `upgrade_on_miss` (was `fallback_not_found`) |
| bridge an older record up to current | `impl UpgradeFrom<Task1> for Task2` |
| serve current data in an older peer's shape | `impl DowngradeFrom<Task2> for Task1` |

## Alternatives

- **A global migration walk / version-upgrade chain** — the classic approach,
  rejected as the very friction [RFC 0004](0004-struct-hash-and-schema-evolution.md)
  removes. Here the walk is *per-record, lazy, bounded by declared versions*, not a
  stop-the-world sweep.
- **Type-erasing the pivot reference** (store an opaque `PivotId`, drop the member's
  type from the holder's hash) — considered and rejected: the numbered-type + alias
  keeps holder hashes stable *and* keeps the member's type visible at the call site,
  and SALT-swap gives per-version addressing that erasure could not.
- **Rewriting the pivot in place / re-pointing the holder** — rejected: it destroys
  the frozen history snapshot and forces holder writes on every member evolution.
  SALT-derived addressing avoids both.
- **A compile-time forced cascade** (a member edit bumps every ancestor's hash to
  the `Unique` root) — rejected: the alias demotes it to a runtime, per-collection
  detection plus an *optional* advisory warning.
- **An identity-pin attribute** (`#[wavedb(identity = "Task")]` to freeze a renamed
  snapshot's hash) — an earlier draft; superseded by numbered types, which freeze
  the old hash naturally (a never-renamed `Task1` keeps its hash with no attribute).
