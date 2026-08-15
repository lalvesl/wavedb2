# RFC 0049 — Elastic pages and load-driven splits

- **Status:** Planned
- **Builds on:** [RFC 0018](0018-storage-engine.md) (linear-hashed page
  directories), [RFC 0041](0041-single-barrier-checkpoint.md) (splits decided in
  the plan, before anything is written)
- **Crates:** `wavedb-storage`
- **Code:** `crates/wavedb-storage/src/{plan,directory,page_store}.rs`

## Summary

A page occupies whatever its serialised image needs, and nothing ever tries to
bring it back under a limit. `split_threshold_blocks` stops being a per-page
invariant to enforce and becomes the **target mean load** that paces linear
hashing's split pointer.

A bucket over target simply spans more blocks until its turn comes round. A
bucket that splitting can never relieve — one holding a record larger than the
target — stays large permanently, which becomes a supported outcome instead of
an unbounded loop. There is **no cap**: a large object is just a large page.

## Motivation

### What pages already do

Pages are already variable-size, and the descriptor already says so:

- `plan.rs`'s `blocks_of(bytes) = bytes.len().div_ceil(BLOCK_SIZE)`, and
  `checkpoint.rs`'s `size_of` allocates exactly that many blocks per page;
- `BlockDescriptor` packs `[start u40][count u20][occupation u4]` — the span is
  in the descriptor, and `occupation_of` already records how full the run is;
- `DEFAULT_SPLIT_THRESHOLD_BLOCKS = 8` is **32 KiB**, and it is a threshold, not
  an allocation unit.

So a bucket holding one 50-byte record occupies **one 4 KiB block**. The only
waste left is sub-block, and that is the engine's quantum everywhere (see
Alternatives). This RFC changes nothing about sizing; it removes the machinery
that fights it.

### The defect: a local trigger driving a globally-ordered action

Linear hashing's split order is not a choice. `directory.rs`:

```rust
pub const fn next_split_bucket(dir_len: u64) -> u64 {
    dir_len - (1u64 << dir_len.ilog2())
}
```

The pointer is *derived from the directory's length*, because addressing
(`bucket_index(len, hash)`) depends only on that length. There is no way to
split bucket K out of turn — and therefore no way to relieve a specific bucket
on demand.

The trigger, however, is per-page (`plan.rs`):

```rust
if !pages.values().any(|b| blocks_of(b) > self.split_threshold_blocks) {
    return Ok(());
}
let source = dir.next_split_bucket();   // not the bucket that overflowed
```

So one over-sized bucket makes the loop split whatever bucket is next, over and
over, until the pointer arrives. Each of those turns:

- reads a bucket nobody touched (`staged_page` → `read_page`, one IOp);
- inserts **two** pages into the round's window;
- grows the directory by one bucket.

With a uniform hash the offender sits an expected N/2 turns away — at 131 072
buckets, ~65 536 wasted splits, paced 64 per round by `MAX_SPLITS_PER_ROUND`.
The budget bounds the burst, not the total.

### The case that does not terminate

Splitting distributes whole records; it cannot divide one. So a bucket holding a
single record larger than the threshold is **permanently** over it — including
after the pointer finally reaches that bucket and splits it, since every
colliding-by-size record lands on one side.

The condition `any(over threshold)` is then never false. Every settle round that
touches that bucket burns its entire 64-split budget and grows the directory by
64 buckets, and does so again on the next such round, indefinitely. A 40 KiB
record — a blob field, a long `Vec` — is enough to trigger it.

The existing comment says "page size is a target, not an invariant", and the
code then spends unbounded IO trying to make it one.

## Design

### A page is its image

Drop the enforcement. `pages[bucket]` occupies `blocks_of(image)` blocks — as it
does today — and no loop tries to reduce it.

**No cap.** The only ceiling is structural: `count` is 20 bits, so a page tops
out at 2²⁰ blocks = 4 GiB. That is what makes a large object simply a large
page, with no out-of-line blob path, no special descriptor, and no second read
path — the record lives in its bucket like any other.

### The threshold becomes a load target

Split when the directory's **mean** page size exceeds the target, not when some
page does. That is the trigger linear hashing is built for: a fixed split order
is only coherent with a global signal, because the order cannot respond to a
local one.

Accounting is O(1), not O(buckets): `Directory` keeps a running sum of its
descriptors' block counts, adjusted in `set_descriptor` and `push_bucket`. On
recovery the sum is derived once from the replayed descriptors.

`MAX_SPLITS_PER_ROUND` stops being a safety valve against a runaway loop and
becomes what its name says — a pacing budget. The loop now terminates because
each split raises the bucket count and so lowers the mean.

### What a split still does

Unchanged mechanics: partition on bit `level`, keep/move, `push_bucket`. Only
the reason it fires changes. The pointer still advances round-robin, and relief
for a specific bucket still arrives only on its turn — which is now acceptable,
because being over target costs bytes on that bucket's own reads and nothing
else.

### Reads

Unchanged in shape: one positioned read of `count` blocks, one IOp whatever the
size. A large page costs bytes and decompression time, charged to the bucket
that holds the data, not to the database.

## What it costs

| | today | this |
|---|---|---|
| split trigger | any planned page over threshold | mean page size over target |
| splits to relieve one bucket | ~N/2, each a read + 2 page writes | none — it grows |
| a record larger than the threshold | 64 splits per touching round, forever | a page that is simply large |
| directory growth | driven by the worst bucket | driven by total load |
| max page size | (unbounded in practice, fought continuously) | 4 GiB, structural |
| read of an over-target bucket | — | 1 IOp, more bytes |
| read of every other bucket | — | unchanged |

The honest cost: a bucket can stay over target for as long as the split pointer
takes to reach it, and every read of it moves more bytes in that window. That is
paid by whoever reads that bucket, in proportion to how oversized it is — where
today the cost is paid by the whole type, in split IO, immediately.

## Alternatives

### Split out of turn

Relieve the bucket that actually overflowed. Impossible without changing the
addressing rule: `bucket_index` depends only on the directory length, so an
out-of-order split would misroute every subsequent lookup. Getting it would mean
extendible hashing — a per-bucket depth and an indirection table — which is an
architecture change, not a policy change.

### Overflow chains

The textbook linear-hashing answer to a full bucket: link an overflow page.
Rejected — a chain is a second IOp on every read that reaches it, where an
elastic page is one read of N blocks. And the engine is copy-on-write: it
rewrites a whole page per change anyway, so the in-place-append advantage that
makes chains attractive elsewhere does not exist here.

### Pack small pages into shared blocks

The residual sub-block waste: a 50-byte page still holds a 4 KiB block.
Rejected — co-tenancy breaks per-page copy-on-write independence. Rewriting one
page would rewrite the block, so every co-tenant's descriptor would have to
change in the same round, coupling unrelated buckets' write paths and turning
free space into a function of page identity. The 4 KiB block is the engine's
quantum everywhere else; paying it per bucket is the price of that uniformity.

### Keep the per-page threshold as a secondary trigger

"Split on load, but also if some page is really large." Rejected: that is the
cascade, reintroduced with a higher constant — and it still never terminates for
a record larger than whatever the constant is.

## Open questions

- **Blocks or occupation?** The load target is proposed over `count`, the blocks
  a page spans. `occupation` (the 4-bit gauge already in the descriptor)
  measures bytes actually used within the run and is closer to true load; it may
  be the better signal, at the cost of reading a coarser number.
- **Per type or global?** The target is per-`StructStorage` today by virtue of
  living on `PageStore`. A type of large records and a type of tiny ones want
  different targets, and both are known at compile time.
- **Does the window still fit?** [RFC 0041](0041-single-barrier-checkpoint.md)
  carves one contiguous window per round, so the largest single page now sets a
  floor on the window size. [RFC 0042](0042-free-space-defragmentation.md) is
  what supplies such extents; a page larger than any hole grows the tail, which
  is the existing fallback and needs no new mechanism — but the interaction is
  worth measuring before the target is tuned.
- **Is a large record a page at all?** Nothing here stops a 1 GiB record from
  being a 1 GiB page, and reading any *other* record in that bucket would then
  move 1 GiB. If that shows up in practice, the answer is a size at which a
  record gets its own bucket-of-one rather than an out-of-line path — but that
  is a separate RFC and should wait for evidence.
