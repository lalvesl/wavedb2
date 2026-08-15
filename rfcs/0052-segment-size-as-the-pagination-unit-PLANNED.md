# RFC 0052 — Segment size as the pagination unit

- **Status:** Planned — opened 2026-07-29, revised 2026-07-30
- **Crates:** `wavedb-core`, `wavedb-macros`
- **Builds on:** [RFC 0050](0050-clustered-record-chains-PLANNED.md) (the segment),
  [RFC 0051](0051-ordered-record-lists-PLANNED.md) (the sparse index),
  [RFC 0049](0049-elastic-pages-and-load-driven-splits.md) (a page is however many
  blocks it needs, read in one positioned read — which is why a segment's *element*
  count can be fixed without capping its size)
- **Constrained by:** [RFC 0053](0053-tenant-fair-cache-retention-PLANNED.md) —
  nothing may be pinned in RAM, so every cost here is quoted cold

## Summary

The developer declares a chain's segment capacity as a **minimum N**, normally the
page size their interface renders. A segment then holds between N and 2N records and
splits at 2N. Reading a UI page costs one segment read, sometimes two, and the
second is nearly always a cache hit because the first read happened milliseconds
earlier in the same pagination session.

Exactness is neither achievable nor wanted: `Collection::search` is an **async
iterable**, so each `.next()` hands up whatever the segment it just read contained
and the layer above decides how many rows to render. Filters make an exact row count
meaningless anyway. What the sparse index adds is the *unfiltered* pager's two
questions — "jump to page k" and "of M" — answered by one descent instead of a walk,
via element counts stored leaf and subtree.

> **Revised 2026-07-30.** The first draft made the declared size exact and promised
> literally one IOp per rendered page. Exact sizing forces a cascading write on
> middle insertions (an element must displace into the neighbour), and the promise
> was false the moment a filter entered the query. N…2N with an async-iterable read
> path is both cheaper to write and honest about what a UI actually asks for.

## Motivation

The read an application performs is not "get a record" — it is "fill this table".
Every layer of a conventional stack turns that into P random reads plus an index
descent, and RFCs 0050 and 0051 exist to collapse the P.

Letting the developer choose the segment's element count makes the collapse land
where it is useful rather than where a byte heuristic happens to put it. A table
showing 50 rows, backed by 50…100-record segments, is one read per page.

RFC 0049 is what makes it safe to fix a count instead of a byte size: a page spans
however many blocks its content needs, with no ceiling, and is read with a single
positioned read of exactly that run. Fifty fat records are still one seek — the
transfer is longer, the IOp count does not depend on record size. Fixing a byte
target instead (RFC 0050's default) makes the element count vary with record size,
which is what a paginated view cannot use.

## Design

### The declaration is a minimum

```rust
#[wavedb(NonUnique, page = 50)]              // the built-in chain
struct Contact {
    #[wavedb::order(page = 25)]              // this ordering's own chain
    name: String,
    city: String,
}
```

Unset, it falls back to RFC 0050's byte target — the right default for a collection
nobody paginates.

**Capacity does not fold into `STRUCT_HASH`.** It is a physical layout parameter,
not part of the type's meaning: it changes neither the wire shape nor any address
derivation (segment ids are minted, never computed from content). So it may be
changed without minting a new type, and the consequence is only that existing
segments keep their old fill until compaction re-levels them. Same stance RFC 0049
takes on page sizing, and the deliberate opposite of `#[wavedb::key]`, which *does*
fold in because it changes addressing.

### Why N…2N and not exactly N

A fixed exact size is the expensive choice, not the precise one. Holding a segment at
exactly N means an insertion into a full one must displace an element into its
neighbour — which may itself be full, so a single insert writes two segments in the
lucky case and cascades in the unlucky one. And middle insertions are not an edge
case: every declared ordering (RFC 0051) is keyed by a domain value, so arrivals land
wherever the value falls.

With a band, an insert is **one segment write** anywhere in N…2N, and only the 2N-th
pays a second one to split. The amortised cost is one extra write per N inserts, and
no insert ever cascades.

The same rule serves the appending case, which is RFC 0050's built-in chain: its key
is the authoring instant and instants only increase, so arrivals concentrate at the
tail, and a tail reaching 2N splits by sealing its older half.

### What that costs a reader, and why it is nearly free

A segment holding 1.5N records does not line up with an N-row page. The second page
of that chain takes the last 0.5N records of the first segment plus 0.5N from the
next — two reads instead of one.

But the first segment was read **one tick earlier**, by the same iterator, for the
previous page. It is in the cache with near-certainty. This is temporal locality
inside a single query, not the residency assumption RFC 0053 forbids: the honest
figure is "one read plus one very likely hit; two reads worst case".

Generally, with occupancy in `[N, 2N]`, a window of P ≤ N consecutive records spans
at most **two** segments — the band is exactly what bounds it at two, since a
segment always holds at least a full page.

### The read path does not count rows

`Collection::search` and friends return an async `Stream`. The chain implementation
should therefore:

1. descend the sparse index once to the first segment the bound admits;
2. yield the records that segment holds, all of them, without regard to any page
   size;
3. follow `next` when the consumer asks for more, and stop as soon as the bound
   fails.

The quantity a caller wants belongs to the caller — the user-side consumer, or a
`#[server]` function shaping a reply. Two consequences worth stating: the storage
layer never reads a segment the consumer did not ask for, and a filtered query
(`search_by`, or any predicate the caller applies) needs no special handling, since
"how many rows survive" was never the chain's business.

This is also why exact sizing was never worth its cost. A filter makes the number of
rows per segment unpredictable, so no layout can guarantee one segment per rendered
page in the general case.

### "Page k" must not be a walk

A chain has no random access: reaching the k-th segment by following `next` is k
reads. That would make page 100 cost 100 IOps.

The fix makes the sparse index of RFC 0051 an **order-statistic tree**. A leaf entry
gains the element count of the segment it names, and an internal entry gains the sum
of its subtree's counts:

```text
leaf entry:      (min sort key, segment id, count)
internal entry:  (separator, child id, subtree_count)
```

Locating global offset *o* is then a single descent — at each level, pick the child
whose running sum first exceeds *o* — costing at most the index's depth, about two
reads for a million records, with nothing assumed cached. Offset pagination costs
what keyset pagination costs.

Subtree sums are why this is a descent rather than a prefix sum. Summing leaf counts
would have to read every entry before *k*: fine if the index were guaranteed
resident, and it is not, so it would be up to every leaf of the index on a cold read.

Maintenance: an insert, removal or split rewrites the leaf entry **and the counts
along its path to the root** — about two extra nodes, in the same batch, hence the
same window and the same barrier.

### The contract, stated cold

For a chain with minimum N and a UI page P ≤ N:

- **Render a page**: 1 segment read, or 2 when the window straddles a boundary,
  where the first is almost certainly a hit. Rows come decoded from the segment; no
  per-row fetch, because RFC 0050 stores records inline. Without inlining this would
  be 1 index read + P record reads — the inline decision is what the contract rests
  on. "One read" is one **seek**: the bytes transferred and decompressed are the
  whole page the segment sits in, so the segment lane's bucket target should be
  sized near one segment (RFC 0050) or the amplification is
  `bucket_target / segment_size`.
- **Jump to page k**: ≤ 2 reads for the descent, independent of k.
- **Next / previous page**: 1 read, often 0.
- **Total count for the pager**: the root's subtree sum — 1 read cold, 0 warm, and
  exact **for an unfiltered listing**. A filtered total is a scan, here as anywhere.

## Alternatives

- **Exactly N per segment** (the first draft). Precise-looking and more expensive:
  cascading displacement on middle insertions, and the precision evaporates under a
  filter.
- **Engine-chosen capacity** (RFC 0050's byte target alone). Simpler and correct when
  nobody paginates, but the element count then varies with record size, so a view can
  never assume a bounded number of segments per page.
- **A separate pagination index** (offset → segment) instead of counts on the sparse
  entries. Same effect, one more structure to keep consistent; the count is 8 bytes
  on an entry already being rewritten.
- **Cap the element count *and* the byte size**, splitting on whichever hits first.
  Keeps segments from becoming huge with fat records, at the cost of breaking the
  bound exactly when records are large — the case where it is worth the most.
  Rejected: RFC 0049 removed the reason to fear a large page.
- **Offset pagination by walking `next`.** Free to build, O(k) reads. Acceptable only
  for infinite scroll that never jumps.

## Open questions

- **Does `page` belong on the type or on the query?** A schema-level declaration
  serves one UI; two views of the same collection with different page sizes get one
  aligned and one not. A per-view override cannot change the layout, so the answer is
  probably "declare the largest, let smaller views subdivide it" — worth confirming
  against a real application before committing the syntax.
- **Split point.** Halving at 2N gives two segments of N, the minimum — so the next
  insert into either is again a plain rewrite, but both sit at the bottom of the band.
  Splitting 60/40 in the direction inserts are arriving may pay for itself on an
  appending chain. Measure before choosing.
- **Do counts need to survive a crash independently?** They are derived from the
  segments they name, so replay rebuilds them; but if an index entry and its segment
  could ever disagree, a pager would be silently wrong. The single-batch rule should
  make it impossible — worth an explicit test rather than an argument.
- **Compaction and stable pagination.** Re-levelling moves boundaries, so a cursor
  held across a compaction may skip or repeat rows. Keyset cursors are immune (they
  name a sort key); offset cursors are not. Does compaction defer while a watch holds
  an offset cursor, or is "offset pagination is not stable under concurrent writes"
  the documented answer, as it is everywhere else?
