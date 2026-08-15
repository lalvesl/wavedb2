# RFC 0050 — Clustered record chains (B+trees become opt-in)

- **Status:** Planned — opened 2026-07-29, revised 2026-07-30
- **Crates:** `wavedb-core` (the index and collection layers), `wavedb-macros`
- **Code (today):** `crates/wavedb-core/src/index/{tree,tree_insert,tree_delete,stream}.rs`,
  `collection.rs`, `collection_write.rs`, `collection_keyed.rs`, `collection_recency.rs`
- **Builds on:** [RFC 0049](0049-elastic-pages-and-load-driven-splits.md) (a page is
  however many blocks its content needs, with no ceiling)
- **Completed by:** [RFC 0051](0051-ordered-record-lists-PLANNED.md) (the sparse
  index and further orderings), [RFC 0052](0052-segment-size-as-the-pagination-unit-PLANNED.md)
  (segment sizing), [RFC 0053](0053-tenant-fair-cache-retention-PLANNED.md) (why
  every cost here is quoted cold)

## Summary

A collection's living records are **additionally** stored inline inside a chain of
segments, ordered by the live version's **authoring instant** —
`Metadata.succession`'s `CreatedAt`, rewritten on every save (`metadata.rs:26`), so
the chain is in modification order with the most recently changed record at its
newest end. Each segment is a stored value under its own `Id`, holding between N and
2N records plus the ids of its neighbours; above the chain sits the sparse index of
RFC 0051, one entry per segment.

Because that is exactly the key `recency` uses, **the `recency` log disappears**:
the chain is the modification log, with the records inline instead of references.

The record at its **anchor stays where it is and stays authoritative**: a computed
address, one IOp, the version chain, the `Expect` guard. The chain is a derived,
read-optimised duplicate — byte-for-byte the same record envelope.

What the chain replaces is the `current` B+tree: it is the enumeration, the
membership set, and (through its sparse index) the range descent, so that tree
disappears and liveness moves into the anchor's `Metadata`. `dead` becomes a chain of
the same shape with references instead of records. A dense B+tree exists only where
the developer declares one (`#[wavedb::pivot(field)]`, unchanged syntax, unchanged
meaning).

The trade: **a scan costs one read per segment instead of one page read and one
zstd decompression per record; every point lookup, history walk and conflict guard
is untouched; the price is storage and the bytes of a duplicated write.**

> **Revised 2026-07-29.** The first draft *moved* records into the chain, costing
> the computed address for live lookups and forcing two compensations: an implicit
> anchor→segment index for keyed types, and a `Store`-level answer for
> segment-granular `Expect`. Keeping the anchor copy deletes both.
>
> **Revised 2026-07-30 (a).** The chain was append-only and unordered, which needed
> a back-pointer in `Metadata` and gave up the range descent. Ordering it by a key
> the record already carries removes the back-pointer (position is derived) and
> restores the range descent. Splits became legal everywhere, so the asymmetry the
> previous revision had to explain is gone.
>
> **Revised 2026-07-30 (b).** The ordering key moved from the *anchor* to the live
> version's authoring instant, which **absorbs the `recency` log entirely** — one
> structure, one lane and one write per mutation fewer. Its two consumers (catch-up
> and the instant floor) are served better by the chain than by the tree they used.
> The `Pivot` now also holds each chain's **head and tail**, and both ids are
> permanent, so a growing chain never rewrites the `Pivot`.

## Motivation

### What a scan costs today

`Collection::all` is `search(Bound::All)`: descend the `current` tree, yield `Id`s
in `CREATED_AT` order, and for each one call `store.get_of(STRUCT_HASH, id)`
(`collection.rs:314`). The tree half is cheap — `DEFAULT_LEAF_CAP` is **1819**
entries (`index/tree.rs:40`), so ten thousand records are six leaves.

The fetch half is where the IOps go. Records of a type are routed to a page by
**SeaHash over the `Id`'s 16 bytes**, seeded per database (`directory.rs:12`). So
consecutive ids in tree order land in unrelated buckets: the scan touches the
type's pages in random order. And a read that misses the per-type record cache
"already pulls, decodes and discards a whole page" to serve one record — RFC 0044's
own summary of today's read path. Pages are optionally dictionary-compressed
(`page.rs:11`), so that discarded work includes a zstd decompression.

A cold scan of a collection larger than the record-cache budget therefore costs
**one page read and one page decompression per record**, for a collection that
physically occupies a few dozen pages. The amplification is not in the index — it
is in the *placement*.

The browser makes it starker. `wavedb::cache::IdbStore` is a flat `Id → Vec<u8>`
map and every `get` is an async round trip; ten thousand of them to list a
collection is the difference between usable and not.

### This is a read optimisation, and only a read optimisation

Since the anchor copy stays, an insert still dirties the anchor's hash-designated
bucket exactly as today, *plus* the chain's segment. Bulk inserts still scatter
across as many buckets as they do now.

What does not get worse is the **barrier** count: it is one either way, because RFC
0041 collapses a batch into one contiguous window write. The extra copy is paid in
bytes inside that one window, not in seeks. The write side is unchanged in IOps and
larger in bytes — the stated bargain, not a regression to explain away.

### Why the obvious answer is not enough

A page cache (RFC 0044) keeps the pulled page so the scan's siblings come free. It
helps and should still happen. But it only *caches* an access pattern that stays
random: a collection larger than the budget evicts pages before their siblings are
wanted, and the browser store has no pages to cache at all. Clustering removes the
randomness instead of paying to tolerate it.

## Design

### The segment

One stored value, under its own `Id`, holding whole records:

```text
Segment<P> {
    prev: Option<LocalId>,          // toward smaller keys; None = the head
    next: Option<LocalId>,          // toward larger keys;  None = the tail
    entries: Vec<(SecKey, P)>,      // sorted by key
}
```

**Every chain keys by `SecKey`** — the existing `{ field: Vec<u8>, rec: LocalId }`
(`index/node_key.rs:73`), whose trailing anchor already "makes entries unique when
many records share one field value". The modification-ordered chain puts the instant
in `field`, which is byte-for-byte what `recency` does today
(`collection_recency.rs:31`); a declared ordering puts the encoded field value there.
So there is one key type across every chain and every sparse index, and no new key
machinery at all.

What varies is the *payload*: record bytes in a `SEG(T)` chain, nothing in the `dead`
log, where the anchor in the key is the whole entry.

Doubly linked: `next` lets a scan run forward from a descent, `prev` lets a
catch-up run backward from the tail and stop early.

**The payload is the anchor's bytes verbatim.** A record's stored envelope
(`[STRUCT_HASH][meta_len][Metadata][body]`) is copied into the segment unchanged,
so there is exactly **one** record format on disk, the duplicate is byte-comparable
to its source (which makes the consistency invariant testable rather than
arguable), and the per-type zstd dictionary compresses both equally well.

A segment is an ordinary `WaveWire` value stored under an `Id`, so the page
directory, RFC 0049's elastic sizing, the dictionaries and the checkpoint all treat
it as another record of another type: the mechanism lives in `wavedb-core` above the
`Store` seam, which is what keeps it portable to IndexedDB unchanged. One tuning
knob in `wavedb-storage` does move — see [the bucket target](#one-storage-change-after-all-the-bucket-target).

### Segments are their own type, so pages stay homogeneous

A segment's `Id` is `key_nanos()` as the key and the type discriminator of a
reserved segment hash as the salt — precisely the device `BpTree` nodes already use
(`index/node.rs:124`):

```rust
LocalId::new(key_nanos(), true, type_salt(SEGMENT_STRUCT_HASH))
```

The salt puts segments in their own lane of the flat keyspace, so a segment id can
never equal a record anchor, an archive slot, or a tree node, whatever the
timestamps do. It also routes them into their own storage directory, which
**preserves the existing invariant that a page holds records of exactly one
`STRUCT_HASH`** (`page.rs:3`) — anchors, log entries and segments never share a
page. That is worth keeping deliberately: homogeneity is what makes a per-type
zstd dictionary work, and mixing kinds in a page would buy nothing, since hash
placement means co-location cannot be arranged anyway.

**Two reserved lanes per user type:**

| lane | holds | entry payload | index | growth |
| --- | --- | --- | --- | --- |
| `SEG(T)` | the modification-ordered chain + every declared ordering | records, verbatim envelopes | sparse, one per chain | with the live set |
| `DEAD(T)` | the `dead` chain | `[instant][anchor]` | **none** | forever |

They separate on three counts. **Content:** a dictionary is per directory, and fat
record segments trained together with skinny `[instant][anchor]` pairs give a
mediocre model for both. **Lifetime:** the live chains are bounded by the live set;
`dead` is an append-only removal log that is never pruned — bytes are never
destroyed — so it grows without bound, and sharing a directory would let the
unbounded structure drive the bucket splits, diluting the bounded one across an
ever-growing bucket array. **Temperature:** `dead` is written on removal and read
almost never, so mixing would put cold entries in the pages a live read pulls.

Separating also lets the tuning differ: `dead`'s only read is a sequential tail
scan, so it wants larger segments and a larger bucket target than anything else
here.

Within `SEG(T)`, every chain shares one encoding — same key type, same payload type —
so no discriminator is needed: which chain a segment belongs to is decided by the
`Pivot` root that reaches it. Per type rather than one global segment lane (which is
what `BpTree` nodes do today): a segment carries user record bytes, so a per-type
dictionary is worth real compression. The cost of the split is one extra per-type
`StructStorage` static (`no dyn` means every lane needs its own generated slot —
visible in `scripts/registry_size.sh`).

### Why `dead` needs no index

Removed data does not need good read performance — but the reason it can drop the
index is structural, not a concession. Nothing ever *searches* `dead`:

- **"Is this record dead?"** is answered by the liveness field in the anchor's
  `Metadata`, one read, because the record's bytes are never destroyed and
  `Collection::get` still resolves them by address.
- **Revival** (a keyed insert at a dead anchor) does not consume the dead entry —
  the removal stays as a historical event, so there is nothing to look up.
- **`remove`** appends at the tail, which needs only the tail pointer.
- **Catch-up** scans backward from the tail until instants fall below the client's
  cursor. No descent: the walk's length is proportional to how much was removed
  since the cursor, which is exactly the payload the client is about to receive.

So `dead` needs append-at-tail and a bounded backward scan, both pure chain
operations. `recency` keeps its sparse index because it needs the one thing `dead`
does not: **deletion by key.** A save re-keys a record's recency entry from its old
instant to the new one, and a record untouched for a year has its old entry deep in
the chain — one descent with the index, a long walk without it.

### One storage change after all: the bucket target

A page is a bucket holding **whatever hashes into it** — `SlotPage` is an
`Id → bytes` map (`page.rs:89`) and segment ids are minted, so they scatter
uniformly. With the current 32 KiB bucket target
(`DEFAULT_TARGET_BLOCKS_PER_BUCKET = 8`, `page_store.rs:88`), a page holds ~32 KiB
of segments: a 10 KiB segment shares its page with two unrelated neighbours, and
reading it reads and decompresses all three.

So "one read per segment" is one **seek**, with byte and CPU amplification bounded
by `bucket_target / segment_size`. The fix is to let a segment lane carry its own
target, sized near one segment, so a bucket holds about one — average ~1:1 with a
short Poisson tail instead of a fixed 3×. `target_blocks_per_bucket` is already a
field on the store and `Directory` already "carries the per-type policy"
(`directory.rs:8`), so this is a small change — but it **is** a change, and it
corrects this RFC's earlier claim that `wavedb-storage` is untouched.

### Ordered by the modification instant, and split at 2N

The sort key is `(live version's authoring instant, anchor)`: the instant from
`Metadata.succession` (`metadata.rs:26`), the anchor breaking ties and identifying
the record. `mint_instant` is strictly monotone per collection, so ties cannot
actually occur — the anchor is there for uniqueness and because it is the payload
identity anyway.

Ascending by that key, the chain's **head** holds the least recently modified
records and its **tail** the most recent. Both shapes behave the same way, which the
anchor key could not manage: a keyed type's anchor is a content hash, so an
anchor-ordered chain put it in a meaningless order, while modification order is
meaningful for every shape.

Every mutation lands at the tail, because every mutation stamps a fresh instant:

- an **insert** appends at the tail;
- a **save** removes the record from wherever it sat and appends it at the tail —
  one descent to find the old position, two segment writes;
- a **remove** deletes it from its segment and appends to the `dead` chain.

A segment holds between N and 2N records and **splits when it reaches 2N** (RFC
0052). Since arrivals concentrate at the tail, the tail is the usual splitter, and
its split seals the older half.

### No back-pointers, anywhere

Every chain is sorted by a key the record already carries, so **a record's position
is derived, never stored**: to find, update or remove a record's copy, descend the
sparse index with its key. Nothing points *at* a record, so nothing breaks when a
split moves it.

This is what the 2026-07-30 revision bought. The previous draft's unordered chain
needed a `Metadata` back-pointer, which in turn forbade splits (a split invalidates
every pointer to the half that moved) and forced the append-only rule.

### The chains, and the `Pivot`

Two roots remain where there were three, and each is a **pair of endpoints** rather
than a single pointer:

| Root | Today | Under this RFC |
| --- | --- | --- |
| `current` | B+tree of living `LocalId`s | record chain keyed by modification instant: `(head, tail)` **+ sparse index** |
| `recency` | `[instant BE][anchor]` tree, one entry per living record | **gone** — absorbed by the chain above |
| `dead` | `[removed_at BE][anchor]` tree | log chain keyed by removal instant: `(head, tail)`, **no index** |

#### Head and tail, both in the `Pivot`

Each chain carries both ends, so picking a direction costs nothing: ascending starts
at `head`, descending at `tail`, and neither needs a descent to find where to begin.
That matters most for the two hottest reads, which both want the newest records
first — the default listing and the catch-up scan. Both become "read the tail id from
the `Pivot`, one segment read, walk `prev`".

**Both ids are permanent.** A split may always assign the *new* id to the interior
side and let the endpoint keep its own: a head split keeps the lower half at the
head, a tail split keeps the upper half at the tail, an interior split is interior
either way. So once a chain has two segments, its `head` and `tail` never change
again — new ids only ever appear in the middle.

The consequence is an IOp saved on the write path: today "the `Pivot` is rewritten
only when a `BpTree` root moves", and a growing tree moves roots often. A growing
chain never moves its endpoints, so **the `Pivot` is written once at creation and
then essentially never** — not on a split, not on an append, not on a rebalance.

Two details complete the invariant. With a single segment, `head == tail`, and the
first split is the one moment an endpoint id changes — after that they are frozen. And
an emptied chain keeps its last segment as an empty shell rather than dropping it, so
the ids survive even a collection that loses every record.

#### Liveness, which the `current` tree used to answer

Today it is membership: living records are in `current`, removed ones in `dead`. It
becomes a field of the anchor's `Metadata`, answered by the same single read that
fetches the record — strictly cheaper than today for the operation that asks most
often. A keyed upsert currently descends the `current` tree to ask `contains` before
every write (`collection_keyed.rs:55`) and would now read the anchor it is about to
write anyway: absent → first version, living → chained save, dead → revival, one
read, no index.

#### What happened to `recency`

`recency` was never a liveness list, whatever its name suggests. It had exactly two
readers, and the modification-ordered chain serves both better:

| reader | with `recency` | with the chain |
| --- | --- | --- |
| **`Changes` catch-up** (`expose_changes.rs:99`) | descend the recency tree to the cursor instant, walk its tail, then fetch each record by anchor — one random read per change | read `tail` from the `Pivot`, walk `prev` until instants fall below the cursor — the records are *inline*, so no fetch at all |
| **The instant floor** (`collection_recency.rs:60`) | `max_key` descent of the recency tree | the tail segment's last key, an O(1) endpoint read |

So the structure is not dropped for being useless — it is dropped because the chain
*is* it, with the records inline instead of references. The write path loses a whole
re-key: a save used to rewrite the record, remove one recency entry and insert
another; now it moves the record itself, and the log is the movement.

The instant floor becomes `max(live chain's tail instant, dead chain's tail instant)`
— two endpoint reads instead of two tree descents, still strictly monotone against a
rewound clock, which is the guarantee that keeps a cursor a client already advanced
past from ever being authored below.

What no other structure could have served is worth keeping on the record, because it
is the reason this key had to be the modification instant and not the anchor:
**insertion order is not modification order.** A record inserted a year ago and saved
five minutes ago sits deep in an *anchor*-ordered chain — its anchor key is the
year-old insert instant — yet a catch-up from ten minutes ago must return it. Keying
the chain by modification instant is what lets one structure do both jobs; keying it
by the anchor forced a second one.

### The anchor keeps everything that depended on a computed address

- **`Collection::get(store, id)`** resolves by direct address, for live *and*
  removed records — what makes history navigable (`collection.rs:245`).
- **Archives** are written to their derived slots exactly as before: links are
  instants, addresses are computed, no archive is ever repointed.
- **`#[wavedb::key]`** anchors keep being *addresses*: `insert` computes the content
  anchor and goes straight to it. No implicit index, no chain walk.
- **`Write::Expect`** guards the anchor's bytes, so the conflict unit stays one
  record — no false sharing between records sharing a segment, no change to `Store`.
- **Declared `#[wavedb::pivot(field)]` trees** keep their syntax and their current
  value, field bytes → anchor. A hit resolves in one read at the anchor, so an entry
  carries nothing new and `IdStreamExt`'s set algebra is unchanged.

#### The solitary anchor is what lets `dead` stay skinny

The chains hold the **live** set, so a removed record leaves every one of them. Its
bytes then live in exactly one place: its anchor. That is not a convenience — it is
what makes the removal log affordable.

Without the anchor copy, a `remove` would have to either leave the record in a chain
(contradicting what the chain *is*) or copy its bytes into the `dead` log. And `dead`
is the one structure that **grows forever** — bytes are never destroyed, so nothing
prunes it. A fat `dead` log would accumulate every version of every record ever
removed, in a lane no reader wants to page through.

So the anchor lane carries what nothing else can:

- **removed records' bytes**, keeping `Collection::get` honest after a `remove` and
  letting `dead` be `[instant][anchor]` and nothing more;
- **archived versions** at their derived slots, and the `Succession` chain that walks
  them;
- the **`Expect`** target, which is why the conflict unit is one record.

Which reframes the duplication: the anchor is not a copy of the record, it *is* the
record. The chains are derived read paths over it.

### What this gives up

- **Space, and write bytes.** Two copies of every living record instead of one,
  segments between half and fully packed, holes left by removals until compaction,
  and a whole-segment rewrite per mutation in the CoW window. A save carries the
  record's bytes twice. Accepted deliberately.
- **A derived copy can disagree with its source.** An invariant that did not exist
  before. The single-batch rule makes it structural — anchor, segment, index and
  logs land in one atomic `Store::apply` or none of them do — and the verbatim
  envelope makes it byte-testable.
- **A split is a second segment write.** Amortised one per N inserts, and RFC 0052
  argues the N…2N band is what keeps it from being every insert.
- **A save relocates the record**, since its instant changes: one descent to find the
  old position, two segment writes. Against today's save — rewrite the record, remove
  one `recency` entry, insert another — it is a write *fewer* and a structure fewer,
  so the relocation pays for itself. But it is a bigger move than before: whole record
  bytes rather than a 18-byte log entry.
- **`all()` and `search()` change meaning.** They stream in `CREATED_AT` order today,
  where `CREATED_AT` is the *insertion* instant; they will stream in modification
  order, newest first. For a keyed type this is a strict improvement (its
  anchor-ordered walk was in meaningless hash order). For a time-keyed type it is a
  real semantic change to a public API, and the honest framing is that
  "most-recently-changed first" is what a listing usually wants — but a caller who
  needed insertion order must now declare an ordering to get it.

Note what is *not* given up: with a sparse index above the chain, `Bound::Range`
keeps its logarithmic descent (`index/mod.rs:64`) and gains a dense scan. The first
draft's main regression is gone.

## Alternatives

- **Page cache only (RFC 0044).** Cheaper and strictly additive, but pays to
  tolerate a random access pattern instead of removing it, and does nothing for the
  browser store. Should still land; not a substitute.
- **A forwarding stub at the anchor instead of the record.** Keeps every address
  computation valid and makes `get(id)` two reads instead of one. It saves the
  duplicated bytes and buys nothing here: the stub dirties the same anchor page the
  full copy would, so the write cost is identical and only reads got worse.
- **Move the records, compensate with indexes** (the first draft). Saves one copy,
  and pays in the `Store` seam, in `#[wavedb::key]`'s addressing story, and in a
  per-record index the duplicate makes unnecessary. Rejected in favour of spending
  the space.
- **Unordered append-only chain** (the first draft's chain). Simplest to write and the
  cheapest insert, but needs a stored back-pointer per record and gives up the range
  descent. Rejected once a key the record already carries turned out to be free.
- **Keyed by the anchor** (the second draft). Preserves today's `all()` order exactly
  and never relocates a record on save, since the anchor is immutable. Rejected
  because it cannot absorb `recency`: insertion order is not modification order, so a
  second instant-keyed structure stays mandatory, with its own lane, its own index and
  a re-key on every save. It also leaves keyed types walking in hash order.
- **Leave it alone and widen the record cache.** Trades the scarce resource (RAM)
  for the abundant one — backwards.

## Open questions

- ~~**Is the anchor copy ever worth eliding?**~~ **Resolved (2026-07-30): no.** The
  earlier framing imagined a pure log type paying for a copy nobody reads. It misses
  that a removal evicts the record from every chain, so the anchor is the only
  remaining home for its bytes — eliding it would force the `dead` log to carry
  records, and `dead` is the one structure that grows forever. Add `Collection::get`
  being public API for every shape, and the anchor is load-bearing in all cases.
- **Compaction trigger.** By occupancy ratio, by absolute hole count, or on the same
  maintenance tick as defragmentation (RFC 0042) — and whether it may run while a
  watch holds a cursor into the segments it is rewriting.
- **Migration.** None, by policy — `FORMAT_VERSION` is pinned and old `data.bin`
  files are unsupported pre-release. Named because this is the largest on-disk
  layout change since the anchor restructure.
