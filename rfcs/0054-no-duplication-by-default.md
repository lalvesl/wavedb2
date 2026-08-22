# RFC 0054 — No duplication by default

- **Status:** Implemented 2026-07-31 — opened 2026-07-30 as "the anchored layout
  as a declared alternative to clustering", then **inverted** on the same day
  (see below): it is not an alternative, it is the default, and there is no knob.
- **Crates:** `wavedb-core`, `wavedb-macros`
- **Supersedes the default of:** [RFC 0050](0050-clustered-record-chains.md)
  (which made every collection carry a second inline copy of every record)
- **Completed by:** [RFC 0051](0051-ordered-record-lists.md) — a declared list is
  now the *only* way to ask for duplication

## Summary

A record lives at its **anchor** and nowhere else. Every collection carries two
chains keyed by instant — **recency** and the **removal log** — and they are the
same shape: ids and nothing else. A `SecKey` already carries `rec: LocalId`,
which is the anchor, so there is nothing left to put in the payload. One segment
read gives membership and order; each record is then resolved at the address it
always had.

Naming matters here, because the corpus spent a while calling recency "the record
chain" — a name it had earned when it held records inline and lost when it stopped.
What it is now is exactly the removal log with a different question: *what changed*
instead of *what died*.

`#[wavedb::list(...)]` is the opt-in to duplication. Each declaration adds one
more chain, sorted by the declared property, this one holding whole records
inline — which is what buys a dense bulk read, at the cost of a full copy.

| declaration | copies of each record on disk |
| --- | --- |
| (none) | **1** — the anchor |
| K × `#[wavedb::list]` | **1 + K** |

## Why there is no knob

The RFC opened proposing `#[wavedb(NonUnique, layout = anchored)]`, and that
spelling was built and then deleted the same day. Two observations killed it, both
the reviewer's:

**1. A chain is a linked list, and `Chain<P>` is already generic over its
payload.** The first draft of this RFC described "one dense B+tree" over the
records and an implementation followed it literally: a second root kind on the
`Pivot`, a second set of lanes, a second engine path — and, from the B+tree's
forward-only walk, an invented "ordering blocker" about `all()` coming out
oldest-first.

None of it was necessary. The removal log has been `Chain<()>` — a chain of
pointers with no payload — since RFC 0050 phase 3b. So the no-duplication model is
**the same chain with an empty payload**, and everything follows for free: the
chain is doubly linked with both `head` and `tail` in the `Pivot`, so it still
reads newest-first by walking `tail → prev`; `instant_floor` still reads the
endpoints; catch-up still walks it (`expose_changes::tail_since` was already
payload-generic); the lanes and the storage slots do not move; and a split still
does not rewrite the `Pivot`, because RFC 0050 gives the growth end its id
permanently.

**2. "Not anchored" is not a state a record can be in.** The record has to be at
its anchor regardless — history resolves it there, the dead log names it there,
and `Collection::get` is a computed address. So `anchored` was never a mode; it
was the only possibility wearing the costume of an option. What the old knob
actually controlled was whether there was an *extra* copy — which is exactly what
`#[wavedb::list]` already controls, per ordering.

Which leaves one axis, not two: **every ordering is a chain, and a chain either
carries records or does not.** The built-in one (keyed by modification instant) is
the ordering you always get and never carries them; a declared list is an ordering
you ask for and always does.

## Motivation

RFCs 0050–0052 optimise for a collection that is **read in bulk**, and made that
bargain for everyone: records inline in chained segments, a second copy of every
record whether or not anything ever lists them. That is the wrong default.

| | **no duplication** (the default) | **a declared list** (opt-in) |
| --- | --- | --- |
| disk per record | 1 copy | 1 more copy per list |
| write bytes per save | the record, once | + the record's bytes per list |
| bulk read of N records | 1 read per segment of pointers, then N reads | 1 read per segment, records inline |
| single-record read | 1 read — the anchor is a computed address | identical |
| index size | one entry per segment | one entry per segment |

The last row is the one that decides it: a **point-lookup** collection gains
nothing from duplication, because the anchor was always a computed address — so
an audit log nobody lists, a configuration table read by key, a join row probed
one at a time were all paying for a copy no read ever touched.

And the collection that *does* list still gets everything RFC 0050 built: it
declares `#[wavedb::list]` on the property it lists by, and that ordering — not
the incidental modification order — is the one laid out densely. The duplication
lands where the read is, instead of everywhere.

What the pointer chain keeps, for free, is the part every collection needs: it is
the membership set, the modification order, and the "changed since" cursor
(W6/W7 live sync), at ~18 bytes an entry instead of a whole record.

## What landed

All of it, on 2026-07-31, by deletion as much as by addition:

- The built-in record chain is `Chain<()>` — `collection_roots::records_chain`
  opens it at the removal log's capacity, because an ~18-byte pointer entry has
  nothing to paginate. `page = N` on the type consequently governs the
  **lists**, which are the chains that hold records.
- The write paths insert `()`. `plan_chain_move` still encodes the envelope and
  still hands it back, now for one reason instead of two: the declared lists hold
  it, and it doubles as the liveness gate.
- The read paths — `Collection::all`, the wire `All`, and catch-up — take
  membership and order from one read per segment and then resolve each record at
  its anchor.
- The `layout` knob, the `Layout` enum, the `records_tree` root, the second lane
  set and the two contradiction refusals were all **deleted**. There is nothing
  left to declare.
- **Recency moved to a lane of its own** (`Lane::Recency`, tag `WDB.REC`), so a
  NonUnique type now occupies four: declared-list segments, recency, the removal
  log, the sparse index. It shared the record lane while it carried records, and
  the moment it stopped, that sharing became the thing lanes exist to prevent —
  one directory and one zstd dictionary trying to model both ~18-byte id entries
  and segments of whole records. `storage_entries()` is 6.

One test's premise inverted with the default and was rewritten rather than
patched: `a_collection_catch_up_reads_segments_not_records` asserted that
catch-up never resolves a record by address, which is now false by design. It is
`a_collection_catch_up_is_segment_shaped_not_collection_shaped`, and it asserts
what is actually true and actually valuable — every changed record *is* fetched,
and finding **which** ones stays proportional to what changed rather than to the
collection.

Two `page`-layout tests inverted with it, and were rewritten against three
distinguishable capacities (4 → ~10 segments, 16 → 2…4, 256 → 1) so they cannot
pass by coincidence.

## Why `all()` stays recency-ordered

A save moves a record to the front of `all()`, so a listing reshuffles as it is
edited. That was raised as a defect and is not one — it is the feature (reviewer,
2026-07-31): "always list what was modified last" is what a listing usually wants,
and it is the same property that lets one structure serve live sync.

The alternative considered and rejected was keying the chain by `created_at`
instead: stable order, no relocation on save. It loses "what changed since",
which is not optional, so it would need recency back as a *second* chain — and
then the collection carries two structures to answer what one answers now. The
stable, domain-meaningful order a caller actually wants is almost never
"insertion order" anyway; it is "by name", "by due date", "by city" — which is
what `#[wavedb::list]` is for.

Worth recording because it nearly went the other way, and the obstacle that
would have surfaced later: for a `#[wavedb::key]` type the anchor is a **content
hash**, so it carries no creation instant at all, and `Metadata` stores the live
version's instant and its predecessor's — never the first. A `created_at`
ordering would have been well defined for one shape and silently undefined for
the other, until creation was promoted to a stored field.

## Open questions

- **Should `#[wavedb::pivot(...)]` be absorbed?** A secondary index already *is*
  "an ordering by a field, pointers only" — the same thing a payload-free chain
  is, implemented as a dense `BpTree` instead. Unifying them would leave one
  concept (an ordering, which does or does not carry records) where there are now
  two spellings. Not done here: a pointer chain and a dense tree are **not**
  equivalent for exact-value lookup, which is what `pivot` is for, so the merge
  needs its own cost argument and its own RFC.
- **The removal log's key.** It is keyed by the *removal* instant, which is what
  answers "removed since \<cursor\>". The only record data it holds is the anchor
  — which, for a NonUnique record, is its `CREATED_AT`. Worth restating because
  the two instants are easy to conflate.
