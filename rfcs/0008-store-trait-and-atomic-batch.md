# RFC 0008 — The Store trait and the atomic batch

- **Status:** Implemented
- **Crates:** `wavedb-core`
- **Code:** `Store` trait, `Overlay`, `core::notify` (`crates/wavedb-core/src/`)

## Summary

`Store` is the one abstraction every backend implements — the native `PageStore`
([RFC 0018](0018-storage-engine.md)), the browser `IdbStore`
([RFC 0025](0025-wasm-indexeddb-target.md)), the client cache, and in-memory
test stores. Its surface is small: `get` / `get_of` (read by id, optionally
type-checked) and **`apply` — one atomic batch of writes**. The atomic batch is
*the* consistency primitive of the whole engine.

## Motivation

Every layer above (records, B+trees, collections) must be able to say "these
writes commit together or not at all" without knowing whether "together" means a
journal frame (native) or an IndexedDB transaction (browser). Making the atomic
batch the trait's unit pushes that difference entirely below the seam: the
`Store` trait absorbs the native/browser split, so `BpTree` and `Collection` are
written **once**, `Store`-generic.

## Design

- **`apply(batch)` is all-or-nothing.** Native, that is journal-first WAL
  (append + cache commit under the journal lock); wasm, one IndexedDB readwrite
  transaction (complete = durable, error/abort = rolled back whole). Either way
  the caller gets the atomic-batch contract for free.
- **`Write::Expect` guards.** A batch may open with a compare-and-set guard
  validated pre-batch under the commit lock and **stripped before journaling** —
  the concurrency primitive behind conflict-safe saves
  ([RFC 0009](0009-anchors-succession-and-history.md)).
- **`note_mutation` — the notification seam.** After the one atomic `apply`, the
  write path hands the store a `Mutation` (op-level meaning a raw batch can't
  carry: a chained save writes archive records indistinguishable from the live
  one, and a remove may rewrite no record at all). It is a **provided no-op** —
  the closure never even builds the value unless a store overrides it, so every
  ordinary store pays nothing. The node wraps its engine in a store that
  overrides it to route live-sync events ([RFC 0023](0023-quick-node-and-gates.md));
  cache mirrors ride the no-op, so a mirrored write is not a new mutation.
- **`Overlay` — batch-pending read view.** So multiple plans on one tree compose
  into a single atomic batch, `Overlay` presents pending writes as if already
  applied — a save that touches several B+tree nodes reads its own uncommitted
  changes while building the rest of the batch.
- **Blanket `impl Store for Rc<S>`.** A shared store is a store, so a wrapper can
  own its backend by value while a maintenance handle keeps its own clone.

## Invariant it enforces

**Every mutating op is exactly one atomic `apply` batch** — the record write
plus every touched B+tree node plus a `Pivot` rewrite when a root moves. This is
the rule [RFC 0011](0011-bptree-index-and-collections.md) and
[RFC 0009](0009-anchors-succession-and-history.md) are built to satisfy.
