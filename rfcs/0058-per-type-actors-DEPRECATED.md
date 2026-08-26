# RFC 0058 — Per-type actors, and a `Send` engine *(deprecated)*

- **Status:** **Deprecated** — opened 2026-08-01, parked 2026-08-04, superseded
  2026-08-22.
- **Superseded by:** [RFC 0064](0064-pivot-owned-concurrency-PLANNED.md), which
  keeps the actor shape and replaces the unit of ownership; and
  [RFC 0063](0063-engine-yield-map-and-interruptible-engine-PLANNED.md), which
  carries this RFC's *motivation* forward as the yield map.
- **Reduced** to what it proposed, why it lost, and where its surviving content
  went — per [RFC 0000](0000-rfc-process.md) §5. The full text is in this file's
  git history.

## What it proposed

**One actor per type**, owning that type's whole family of storage slots, with
the journal and the writer staying single (one file, one fsync, so a shared
barrier is group commit rather than a bottleneck). The partition was taken from
the storage layout — per-`STRUCT_HASH` page directories, six slots per type —
on the argument that it already existed and merely needed to be reflected in the
concurrency.

It required two expensive things:

- **`Lane::Tree`** — splitting `BPTREE_NODE_STORAGE`, the one process-global
  slot holding every type's B+tree nodes, per type. This is a change to stored
  bytes, so it charged a **`STRUCT_HASH` break for every indexed type** to a
  concurrency refactor.
- **A `Send` engine on native** (user decision 2026-08-01, reversing
  `CLAUDE.md`'s non-`Send` stance): `Store`'s async-fn-in-trait desugared to
  `-> impl Future + MaybeSend`, with a cfg'd bound in `wavedb-platform` so wasm
  paid nothing.

## Why it lost

**Parked 2026-08-04** on a gate of its own naming: the whole engine must be able
to run as a *single actor on a single thread* (the wasm target), and "N actors
as N tasks on one worker" is a different execution model. Behind that: an
unaddressed actor-to-actor deadlock surface, `Lane::Tree`'s price, and
performance claims that were all estimates.

**Superseded 2026-08-22** on the unit itself. The type is the right partition
for *storage* and the wrong one for *ownership*:

- A Pivot instance is created **one per holder**, not one per tenant per type —
  the API imposes no limit and the shop workload nests one `Product` collection
  per `Shopping`. So the type is a *coarsening* of a partition that already
  exists at a much finer grain, and choosing it accepts a hot-type ceiling
  (0058 did accept it) that the finer unit does not have.
- Ownership does not have to follow storage partitioning. The disk sits behind
  a single owner regardless, so the global node slot costs nothing and
  **`Lane::Tree` — with its `STRUCT_HASH` break — buys nothing**.
- Actors own across *threads*; a mailbox in front of a single-threaded engine
  whose locks never contend is a second scheduler on top of the first. A queue
  round-trip costs more atomic traffic than an uncontended `parking_lot` lock,
  not less. (0063.)

Its own open list, resolved rather than inherited: the deadlock surface (#1)
disappears when request/reply is `await` rather than a synchronous message; the
"single-actor collapse has no design" (#2) is void because the base case is not
an actor.

## What survived

- **The motivation** — that `drain`/`settle`/`commit_journal` are synchronous
  `fn`s occupying the only thread, and that single-threadedness silently
  supplies two undocumented invariants (I1: batch application atomic across the
  per-type caches; I2: `pending` empty ⇒ everything settled). Recorded in
  [0063](0063-engine-yield-map-and-interruptible-engine-PLANNED.md), which turned
  it into a map of every blocking site.
- **The routed-write fix.** 0058 sketched putting the `STRUCT_HASH` on
  `Write::Remove` / `Write::Expect` so a write need not be broadcast to every
  mailbox. 0063 promoted it for a different reason — the same scan was a live
  cost on one thread, inside the journal lock — and it **landed 2026-08-21**.
- **The single-writer journal**, which [0064](0064-pivot-owned-concurrency-PLANNED.md)
  keeps verbatim as its writer actor.
