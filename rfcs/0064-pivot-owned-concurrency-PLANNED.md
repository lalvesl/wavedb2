# RFC 0064 — Pivot-owned concurrency

- **Status:** Planned — opened 2026-08-22. Nothing built.
- **Supersedes:** [RFC 0058](0058-per-type-actors-DEPRECATED.md) (per-type
  actors). 0058's *motivation* stands and is recorded in
  [0063](0063-engine-yield-map-and-interruptible-engine-PLANNED.md); its
  central choice — the type as the unit of ownership — is what this RFC
  replaces.
- **Builds on:** [0063](0063-engine-yield-map-and-interruptible-engine-PLANNED.md)
  (the yield map, and the A2 fix that came out of it),
  [0011](0011-bptree-index-and-collections.md) (the Pivot, whose real
  cardinality this RFC turns out to depend on)
- **Crates:** `wavedb-storage` (cache ownership), `wavedb-quick-node` (the
  shards and the disk actors), `wavedb-core` (routing metadata)
- **Code:** `crates/wavedb-core/src/collection.rs`,
  `crates/wavedb-core/src/local_handle.rs`,
  `crates/wavedb-storage/src/struct_storage.rs`,
  `crates/wavedb-core/src/expose.rs`

## Summary

The unit of concurrency in WaveDB is the **Pivot instance** — one collection —
and not the type. Two collections of the same type share nothing: different
Pivot record, different B+tree roots, different chain segments, different list
segments. Only the items *inside* one collection are inherently serial, because
those genuinely share a tree.

This was not visible because the documentation said the wrong thing. Four
places claimed a Pivot is created "one per tenant per type"; the API imposes no
such limit and the shop workload contradicts it directly — each `Shopping`
holds its own `Product` collection, so there are as many `Product` collections
as there are orders. Corrected in this RFC's first commit.

The consequence is large enough to replace 0058's design outright:

- **No hot-type ceiling.** 0058 accepted that a busy type is one core. Under
  Pivot ownership the busiest type in the shop is not one actor, it is one
  actor per order.
- **No balancer.** With millions of naturally independent units,
  `shard = hash(pivot_id) % N` is uniform by construction: a function, not a
  routing table. Nothing splits, nothing migrates.
- **No `Lane::Tree`, and therefore no `STRUCT_HASH` break.** 0058 needed it to
  finish a partition that this one does not need at all.
- **The developer controls concurrency by modelling.** Nesting Pivots declares
  parallel units. That is explainable in a sentence, which range-sharding is
  not.

## Motivation — why the type is the wrong unit

RFC 0058 chose the type because the storage layout already partitions by it
(per-`STRUCT_HASH` page directories, six slots per type). That is a real
partition, and it is the right one *for storage*. It is the wrong one for
ownership, for two reasons.

**First, one slot is global.** Every B+tree node of every tree of every type
settles into a single reserved slot:

```rust
// crates/wavedb-core/src/index/node.rs:41
pub const BPTREE_NODE_STRUCT_HASH: u64 = 0x42_50_54_52_45_45_00_01; // "BPTREE\0\x01"
// crates/wavedb-storage/src/struct_storage.rs:216
pub static BPTREE_NODE_STORAGE: StructStorage =
    StructStorage::without_compression(BPTREE_NODE_STRUCT_HASH);
```

Node ids are unique globally by `key_nanos()`'s fused counter
(`mint_node_id`), not by being scoped to a tree — which is exactly what lets
them share one flat keyspace. Every mutating op touches the index, so under
per-type actors this one slot's owner sees 100% of write traffic. 0058 found
this and named the fix (`Lane::Tree`, splitting the slot per type) while
recording its price: a `STRUCT_HASH` break for every indexed type, charged to
a concurrency refactor.

**Second, and this is what dissolves the first: ownership does not have to
follow storage partitioning.** The disk sits behind a single owner regardless
(see the disk actors below), so the shared *slot* costs nothing. What needs an
owner is the **cache**. A tree's nodes belong to whoever owns that tree, and
whoever mints a node knows which tree it is for — the raw `Id` does not name
its owner, but the caller always does. That is the same shape as 0063's A2
finding, and the same resolution: pass the owner down rather than search for
it.

Once the two axes are allowed to differ — **cache partitioned by owner, page
directory partitioned by type** — `Lane::Tree` has nothing left to buy.

## Design

### The unit, and what it does and does not share

An owner is one **Pivot instance** (a NonUnique collection), or the pair
`(tenant, STRUCT_HASH)` for a Unique type, which has no Pivot.

| | shared between two collections of one type? |
|---|---|
| Pivot record, B+tree roots, recency/dead chains, list segments | **no** — this is the whole point |
| `mem_cache` for the type's `STRUCT_HASH` | today yes; **must be repartitioned by owner** |
| page directory + zstd dictionary for the type | yes — and that is fine, it lives behind the disk actors |
| `data.bin`, the journal | yes — one file, one owner, group commit |

Inside one collection everything is shared and therefore serial. That is not a
limitation to fix: those records share a tree. A developer who wants
parallelism nests Pivots, which is a modelling decision they already make for
other reasons.

### Assignment: a function, not a balancer

```
shard = hash(pivot_id) % N          // N = worker threads
```

No routing table, no split, no migration, no rebalancing pass. Uniformity comes
from there being millions of units, not from anyone measuring load. This is the
direct answer to the objection that sank the range-sharding alternative below.

### One scheme, three granularities

```
tenant             →  distributed shard   (later; the Id already carries TENANT u48)
  pivot instance   →  thread              (this RFC)
    items          →  serial              (inherent — they share the tree)
```

Every Pivot belongs to exactly one tenant (`Collection` holds `tenant: U48`;
`BpTree` is tenant-scoped), so the distributed step is a *coarsening* of the
same hierarchy rather than a second design. Nothing here is rebuilt to take it.

### Routing at ingress

Verified against `Command` (`crates/wavedb-core/src/expose.rs`) and the
generated exec ops (`crates/wavedb-macros/src/exec_ops.rs`):

| command | pivot on the wire | routes by |
|---|---|---|
| `Insert`, `All`, `Listed`, `ListLen`, `Changes` | yes, in the payload | the pivot, directly |
| `Save`, `History` | Unique — none exists | `(tenant, STRUCT_HASH)` |
| `Get` | **no**, only the `Id` | nothing: the record is at its anchor, so the read actor serves it whole |
| `Update`, `Remove` | **no**, only the `Id` | resolved, see below |

`Update` and `Remove` already resolve the pivot by reading the record's
`Metadata` back-link:

```rust
// crates/wavedb-core/src/expose.rs
pub async fn record_pivot<T, S>(store: &S, id: Id) -> Result<LocalId> {
    let bytes = store.get_of(T::STRUCT_HASH, id).await?...;
    let (meta, _) = record::split_record(T::STRUCT_HASH, &bytes)?;
    meta.pivot_id...
}
```

That is a plain KV read by `Id` — no tree, no pivot — so the **read actor does
it without belonging to any shard**: ingress → read actor resolves
`meta.pivot_id` → route. One hop, and the bytes it read are the ones the update
needs anyway, so the decode moves off the shard thread rather than being added.

`Get` on a NonUnique record needs no tree at all, which means the most common
operation never reaches a shard.

### The disk actors, and the Redis lesson

One writer actor (owns the journal fd, the fsync, and therefore group commit)
and one reader actor (owns `data.bin`, and with it the future page cache from
[0044](0044-page-cache-PLANNED-LOW.md)). Single ownership of the reader is what
makes de-duplication possible: two shards wanting the same block produce one
read, not two.

Redis is the instructive precedent and it is usually mis-cited. Its model was
never "one thread does everything" — Redis 6 added I/O threads because parsing
and syscalls dominated, and the *data core* stayed single-threaded. The rule is
**one thread owns the data structure; everything that is not data-structure
mutation leaves it.** Applied here that gives a third candidate beyond the two
above: **zstd**. Decompression is heavy CPU that is not data-structure work, and
it belongs to the read actor, which is already holding the page's bytes.

What does *not* transfer: a Redis op is a hash lookup in the hundreds of
nanoseconds; a WaveDB op is a B+tree descent plus wire decode. One thread
saturates far sooner, which is why the unit has to be fine-grained — and, under
this RFC, is.

### Request/reply without blocking

An operation that needs a block sends to the read actor and does not block; the
result arrives as a message and the operation resumes. That is what `await`
already is, provided each request is its own task inside the shard's
current-thread runtime:

```rust
let bytes = read_actor.get(block).await;   // this task parks; the shard runs another
```

All tasks in a shard share its non-`Send` state through `Rc` with no
synchronisation. The suspended state machine is generated rather than
hand-written, and **0058's unaddressed actor-to-actor deadlock surface
disappears** — it only exists when the message is synchronous.

### The compile-time single/multi switch

The switch is `N`:

- **single** — one shard; the disk actors are inlined as direct calls. This is
  what exists today, and it is what wasm gets.
- **multi** — N shards, one `current_thread` runtime pinned per core, disk
  actors on their own threads.

The shard does not know which it is. Two consequences worth stating because
they reverse what earlier drafts of this work assumed:

- **tokio's work-stealing scheduler is the thing to avoid, not tokio.** Work
  stealing migrates tasks between cores and destroys the locality the design
  exists for — and it is *why* it demands `Send`. N current-thread runtimes are
  supported and are what this wants.
- **`Rc` survives; the engine does not become `Send`.** What crosses threads is
  the *messages* to the disk actors — `Vec<Write>`, a block request — which are
  plain data and already wire types. The `Send` requirement collapses from "the
  whole engine" to "the message types". No `MaybeSend` seam, no desugaring of
  `Store` off async-fn-in-trait.

### What this does to RFC 0063

0063's two invariants land differently:

- **I1 (a batch is atomic across the per-type caches) dissolves.** A batch
  belongs entirely to one owner, so it is atomic by ownership rather than by a
  lock — no seqlock, no epoch, no staged publish. This was going to be the one
  genuinely expensive part of going multi-threaded.
- **I2 (`pending` empty ⇒ settled) is unaffected**; `evict_settled` already
  quiesces writers deliberately.
- **The yield map survives entirely**, and so does the interruptible `drain`:
  the single-shard build still has one thread, and wasm still needs to give it
  up. 0063 Part 3 remains the next executable step and is unblocked by this RFC.

## Alternatives

- **Per-type actors ([0058](0058-per-type-actors-DEPRECATED.md)).** Superseded.
  It relocates the write hotspot rather than removing it (the queue replaces the
  lock; a queue round-trip costs *more* atomic traffic than an uncontended
  `parking_lot` lock, not less), it accepts a hot-type ceiling this RFC does not
  have, and it charges a `STRUCT_HASH` break for `Lane::Tree`.
- **Range-sharding inside a collection.** The design this RFC was drafted as
  first, and it loses on a hard rule: a B+tree spans ranges, so splitting
  ownership forces the index to become two-level — a change to stored bytes, and
  therefore a `STRUCT_HASH` break for every indexed type. It also needs the
  balancer, the routing table and the runtime migration that Pivot assignment
  does without. It remains the answer for one collection too large for a thread,
  and that is a real case, but it is not the opening move.
- **Tenant-sharding only.** Correct and free, but too coarse: single-tenant is
  a stated sweet spot, and there it degenerates to one shard. It is the *outer*
  granularity here, not the unit.
- **Sharing by lock, finer-grained.** Keeps the statics and leaves I1 as
  discipline, which is how it came to be undocumented.
- **`spawn_blocking` around the synchronous engine.** Still a legitimate interim
  native measure for the checkpoint stall, and still does nothing for wasm. It
  composes with this rather than competing.
- **A `static` per actor reached through `unsafe`** (raised during design).
  Rejected twice over: `OnceLock` gives the same access safely for a relaxed
  load, and more to the point per-shard state should not be `static` at all — if
  it belongs to a thread it is a local of that thread's loop, and there is no
  global access left to arrange.

## Open questions

- **The settle path straddles the boundary.** `plan_slot` reads the caches
  (owner-side) and writes pages and the type's zstd dictionary (storage-side).
  A per-type dictionary grown by two shards at once is a genuine conflict.
  Whether settle runs per-shard and hands images to the writer, or the writer
  pulls bytes from shards, is undecided and is the main unresolved design
  question here.
- **`pending` and the ordering of `Touched`** are process-global today. Per-shard
  queues are the obvious shape; how they compose into one checkpoint round is
  part of the question above.
- **The `Expect` guard** is validated under the journal lock against pre-batch
  state. Under this design the guarded records belong to the shard, so it should
  validate locally and the writer only appends — this looks *simpler*, but it
  has not been checked against replay ordering.
- **Cost of repartitioning `mem_cache`.** It is a real edit to
  `struct_storage.rs` and the honest price of the design.
- **Nothing here is measured.** 0058 was parked partly for treating estimates as
  findings; the orders of magnitude quoted in discussion (uncontended lock vs
  channel round-trip) are from the literature, not from this machine, and the
  hot-unit ceiling is not measured at all.
