# RFC 0011 — B+tree index, collections, and Pivots

- **Status:** Implemented
- **Crates:** `wavedb-core`
- **Code:** `BpTree`, `Collection`, `index/` (Pivot, SecKey), `collection_recency.rs`

## Summary

A NonUnique **collection** is reached through a **`Pivot`** (a per-tenant,
per-type handle holding the addresses of index trees) and navigated by a
**`Store`-generic `BpTree`**. The tree holds *addresses* of records, not their
bytes; there is one tree for living data, one for dead, one **recency** log, and
one per declared secondary index.

## Motivation

This is the mechanism behind the layout win ([RFC 0001](0001-vision-and-non-goals.md)):
a collection's members are colocated behind their own index, so one parent's
children are reached without scanning a shared table. B2C means **millions of
small trees** (one per tenant per collection), which drives several design
choices below.

## Design

### Pivot — the addressing handle

```
current:    LocalId               // B+tree of living records
dead:       LocalId               // B+tree of removed records
recency:    LocalId               // instant-keyed log of the living set
permission: Option<PermissionRef> // collection default (per-record Metadata overrides)
// + one LocalId per #[wavedb::pivot(field)] secondary index
```

- **No element counter** — a count would force a `Pivot` write on every
  insert/remove. The `Pivot` is effectively immutable, rewritten **only when a
  B+tree root moves**.
- Created **explicitly** (`create_pivot`), **one per holder**; the holder
  (a Unique struct or a nesting NonUnique) stores the returned `PivotId`.
  There is no per-type limit — `create_pivot::<T>()` is just
  `Collection::<T>::create`, callable as often as the schema nests — so a
  nesting NonUnique mints one collection per record. That is what the
  "millions of small trees" above means, and it is the property
  [RFC 0064](0064-pivot-owned-concurrency-PLANNED.md) builds concurrency on.

### BpTree — Store-generic, keyed by instant

- `BpTree<K: NodeKey>` over any `Store`, with merge/rebalance; nodes are
  `STRUCT_HASH`-headed ([RFC 0010](0010-metadata-and-record-envelopes.md)).
  Two-phase resolution: walk the tree to an `Id`, then fetch the record. Multi-index reads compose result streams with
  `IdStreamExt` — intersect / union / except adapters over `Id` streams.
- **No dedicated one-node-per-page format** — that was measured to waste the
  dominant case (millions of small trees) and dropped
  ([RFC 0031](0031-node-per-page-bptree-DEPRECATED.md)).

### Collection ops (each **one atomic `apply` batch**)

- **`insert`** — mints identity, writes the record, adds it to `current` +
  every secondary + the recency log.
- **`save`** — a chained save ([RFC 0009](0009-anchors-succession-and-history.md));
  re-keys only the secondaries whose fields changed (primary key is the immutable
  `CREATED_AT`), and re-keys the recency log via the superseded instant
  `plan_chained_save` returns (zero extra reads). The `dead` tree is untouched.
- **`remove`** — the **only** op that writes `dead`; deletes from `current` +
  secondaries + recency, keyed into `dead` by removal instant.

### The two system logs (the sync substrate)

Every collection carries two instant-keyed `SecKey` trees in its Pivot:

- **recency** — exactly one entry per **living** record at its live version's
  instant (insert adds, save re-keys, remove deletes);
- **dead** — keyed by removal instant (a removal log in removal order).

A tail scan over both from a cursor is exactly *"changed since"* — the structure
[RFC 0022](0022-live-sync-navigation-catchup.md) navigates. Both logs' maxima
are the **floor** for monotone minting ([RFC 0009](0009-anchors-succession-and-history.md));
`max_key` is an O(depth) rightmost descent.

## Secondary indexes

`#[wavedb::pivot(field)]` / `#[wavedb::pivot((f1, f2))]` add a `BpTree` root to
the Pivot and a typed `by_field` lookup on the collection handle, resolved
two-phase like the primary. `insert`/`remove` update every index; `save` touches
a secondary only if that field changed.
