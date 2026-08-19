# RFC 0054 — The anchored layout as a declared alternative to clustering

- **Status:** Planned — opened 2026-07-30
- **Crates:** `wavedb-core`, `wavedb-macros`
- **Relates to:** [RFC 0050](0050-clustered-record-chains-WIP.md) (the chain that
  replaces the dense `current` tree), [RFC 0051](0051-ordered-record-lists-PLANNED.md)
  (the sparse index over a chain), [RFC 0052](0052-segment-size-as-the-pagination-unit-PLANNED.md)
  (segment sizing and the order-statistic descent)

## Summary

RFCs 0050–0052 optimise for a collection that is **read in bulk**: records inline in
chained segments, `(K+2)` copies on disk, a sparse index above each chain. That
bargain is wrong for a collection that is rarely read, or small, or read one record at
a time — there the duplication is pure cost.

The alternative kept here is the **anchored** layout: records live only at their
anchors, as today, with one *dense* index over them. A developer declares which layout
a type uses.

### On the words

The axis is **duplication**, and it is worth naming carefully because "dense" is easy
to misread as a property of the data:

- **dense index** / **sparse index** describe the *index*'s granularity — one entry per
  record versus one entry per segment. Nothing to do with cardinality.
- **anchored** / **clustered** describe the *records* — one copy at the anchor, versus
  additional copies grouped into chains.

They are not independent knobs, and that is why one declaration covers both: a sparse
index only works over data physically ordered by its key. Records placed by SeaHash of
their `Id` (`directory.rs:12`) have no such order, so an anchored layout can only ever
carry a dense index; a clustered chain is what makes a sparse one possible. Choosing a
layout chooses both.

Where cardinality *does* matter is a different question, and it favours clustering: a
low-cardinality field under a declared ordering (RFC 0051) puts every record sharing a
value in one contiguous run, so filtering on it becomes a dense sequential read.

## Motivation

The anchored layout is not legacy. It is proven, it is already implemented, and its
cost profile is the opposite of the chain's in exactly the way some collections
need:

| | **anchored** (dense index over stored records) | **clustered** (chains + sparse index, 0050–0052) |
| --- | --- | --- |
| disk per record | 1 copy | `K + 2` copies |
| write bytes per save | record + one leaf | record + one segment per chain |
| bulk read of N records | N random reads, one page pulled and decompressed each | N/segment dense reads |
| single-record read | 1 read (or 0 index reads — the anchor is a computed address) | 1 read, identical |
| index size | one entry per record | one entry per segment |

Read the last two rows together and the case makes itself: a **point-lookup-only**
collection gains nothing from clustering, because the anchor was always a computed
address, and pays for every duplicate. An **audit log nobody lists**, a
**configuration table read by key**, a **join table probed one row at a time** — all
of these want one copy and a dense index, not six copies and a segment chain.

The chain's win begins at the bulk read. Below some size it does not exist at all:
a collection whose records fit in one page is one read either way.

## Design sketch

Deliberately a sketch — the RFC exists now to keep the option from being lost while
0050–0052 land, and the syntax should be settled against real schemas.

- **A declaration on the type**, since it decides the physical model:
  something in the shape of `#[wavedb(NonUnique, layout = anchored)]`, with `clustered`
  the default (bulk reads being the common case for a collection a UI renders).
- **It folds into `STRUCT_HASH`.** Unlike RFC 0052's `page` — a layout knob that
  changes no addressing — the *model* changes what structures exist for a type, so
  it is a schema fact.
- **`Collection`'s surface must not change.** `all`, `search`, `search_by`, `insert`,
  `save`, `remove` mean the same thing in both models; only the plans behind them
  differ. Two monomorphized paths, chosen at compile time by the declaration — no
  `dyn`, no runtime branch, per the workspace's dispatch rule.
- **`recency` does not come back — the anchored index *is* it.** Key it by the live
  version's authoring instant (`Metadata.succession`'s `CreatedAt`) and it
  holds exactly one entry per living record at that instant, which is `recency`'s
  definition word for word (`collection_recency.rs:1`). So an anchored type needs
  **one** tree where today's engine has three: no `current` (liveness is a field of the
  anchor's `Metadata`, per RFC 0050), no separate `recency`, and `dead` stays the
  index-less log chain. The same absorption RFC 0050 got from its chain, obtained
  here from the index's choice of key.
- **An insert needs no search.** `mint_instant` is strictly monotone per collection
  (`mint.rs:46`), so a freshly minted instant is greater than every instant already
  in the collection: the entry always belongs at the tree's extreme right edge, and
  the descent that would look for its position can be skipped outright.

  One honest qualification: this is a **fast path, not an invariant.** The client
  cache's `adopt` path imposes the node's `Metadata` rather than minting locally, so a
  mirrored instant is not guaranteed to exceed everything the local index holds —
  which is precisely why `mint_instant` takes a floor at all. So the rule is "if the
  key exceeds the current maximum, insert at the right edge; otherwise descend" — one
  comparison to buy the common case, with the general path still correct underneath.

  A **save** still descends, since it deletes the entry at the record's *old* instant
  before adding the new one, and that old instant may sit anywhere. The old value is
  in the record's `Metadata`, read for free with the record, so it is an ordinary
  keyed delete — the operation a dense `BpTree` is already good at.

## Open questions

- **Per type, or per ordering?** A type might want an anchored primary and one declared
  ordering (or the reverse). Allowing the mix doubles the paths to test; forbidding
  it is a cliff.
- **Is there a size below which the engine should just decide?** A collection under
  one page is one read either way, so the declaration only starts to matter past
  some threshold. An engine that picked automatically would have to change physical
  model at runtime, which the compile-time dispatch rule forbids — so probably no,
  but worth stating why.
- **How much of today's engine survives verbatim?** The anchored layout is close to it
  but not identical: today has `current` + `recency` + `dead`, and the design above
  has one instant-keyed tree + `dead`. So it is *less* code than today, not merely
  code that keeps working — which is the appealing part, and also the part that means
  the collapse has to be written rather than preserved.
