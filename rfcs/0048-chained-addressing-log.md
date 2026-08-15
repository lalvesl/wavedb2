# RFC 0048 — The addressing log as a chain

- **Status:** Implemented (landed 2026-07-29)
- **Amends:** [RFC 0046](0046-directory-deltas-in-the-window.md) (which put the
  chunk list in the `Commit` frame, and recorded this as its escape hatch)
- **Crates:** `wavedb-storage`
- **Code:** `crates/wavedb-storage/src/{edit,commit,journal}.rs`

## Summary

Each `EditChunk` gains a `prev` pointer to the chunk written before it, so the
chain terminates at a snapshot. The `Commit` frame then carries **one address**
instead of the whole list:

```rust
pub struct CommitFrame {
    pub journal_ts: u64,
    /// Raw `BlockDescriptor` of the newest chunk — start *and* block count.
    /// The walk goes back from here to the snapshot.
    pub head: u64,
}
```

The frame becomes a fixed ~25 bytes regardless of how much history the log
holds. Recovery reconstructs exactly the same state from exactly the same
chunks — it just discovers where they are by walking rather than by being told.

## Motivation

The frame is **appended in full at every checkpoint**. Its content is a list of
addresses that has not changed since the last checkpoint except for one new
entry at the end, and the state it names is the same state the previous frame
named. Rewriting the other N−1 entries buys nothing: any of those frames
reconstructs the same directories.

At the current cap of 1024 chunks that is ~8 KB per checkpoint, and across a
whole compaction cycle of N checkpoints the log costs

```
8 + 16 + 24 + … + 8N  =  4N² bytes
```

— **quadratic in the compaction interval**, for a quantity of information that
is linear in it. That is the shape of the defect, and it is why the direct
saving (a few KB per checkpoint) is the least interesting part of this change.

### The cap is what it really costs

Because the frame grows with the interval, the interval cannot grow. And the
interval is what governs how often a **snapshot** — a full image of every
bucket of every type, the O(database) write [RFC 0046](0046-directory-deltas-in-the-window.md)
exists to avoid — has to be written.

For a 200 GiB database (13.1 M buckets, so a 100 MiB snapshot), amortising both
costs over a cycle of N checkpoints:

| | interval N | frame, averaged | snapshot, amortised | **per checkpoint** |
|---|---|---|---|---|
| today (`MAX_EDIT_CHUNKS = 1024`) | 1024 | 4 KB | 100 KB | **≈ 104 KB** |
| the best any list can do (`N = √(S/4)`) | 5120 | 20 KB | 20 KB | **≈ 40 KB** |
| a chain, at the ratio limit | 25600 | ~0 | 4 KB | **≈ 4 KB** |

With a list, pushing the interval to 25600 would cost 4N² ≈ 2.6 GB of journal
bytes per cycle — far more than the 100 MiB snapshot it would save. So the list
does not merely cost its own bytes; it **puts a ceiling on the interval**, and
that ceiling forces the expensive write.

The benefit scales with the database. At 2 GiB the ratio rule binds long before
the count cap does and the chain saves ~20 %; at 200 GiB it is ~26×. This is a
large-database optimisation, and it is the size at which the whole 0046 line of
work was aimed.

## Design

### The chunk points back

```rust
pub struct EditChunk {
    /// Raw `BlockDescriptor` of the chunk written before this one; `0` ends
    /// the walk. A chunk with `prev == 0` is a snapshot — it stands alone.
    pub prev: u64,
    pub slots: Vec<SlotEdit>,
}
```

**`prev` is a descriptor, not an address.** A chunk spans however many blocks
its round needed, so the pointer has to carry the count as well as the start —
which is exactly what a `BlockDescriptor` packs into its u64
(`[start u40][count u20][occupation u4]`). The walk then reads each chunk with
**one positioned read of exactly N blocks**: no probe read to discover a length,
no fixed-size read that might come up short. The `edits` entries in today's
frame are already raw descriptors for the same reason; `head` is one too.

`prev` is `meta.head()` — known before the window is carved, so it is filled on
the *reservation* pass with its real value. It is a fixed 8 bytes and does not
touch [RFC 0046](0046-directory-deltas-in-the-window.md)'s two-pass
shape-stability argument.

The chunk keeps the envelope it already has — `[len u32 LE][crc32][wire]`, the
customary per-page checking for anything written to `data.bin`. `prev` is a
field of the chunk, so it sits inside that crc like everything else, and a
corrupted pointer fails its chunk's check before there is anything to follow.

### `MetaLog` keeps its list — in RAM

The chain removes the list from the *write*, not from the process. `MetaLog`
still holds every live chunk descriptor, because two things need them:

- `runs()`, which contributes to the checkpoint's protected set;
- `record(chunk, full)`, which returns every superseded run when a snapshot
  compacts the log.

It gains a `head` and drops nothing else. `frame()` returns `head.raw()` alone.

### Recovery walks

`load_commit` reads `head`, follows `prev` until it hits `0`, and applies the
chunks **oldest first** — `Replay` is unchanged, still a forward fold over a
snapshot plus deltas.

Two ways to get the order right:

- **collect, then apply** — walk once accumulating descriptors, then read the
  chunks again forward. 2N reads, no extra RAM, and `Replay` untouched.
  Recommended: it is the smaller change, and startup reads are the resource
  this design has already chosen to spend.
- **reverse fold** — apply newest-first with first-writer-wins per bucket. N
  reads, but it needs a seen-set (a bitset per type, ~1.6 MB at 13.1 M buckets)
  because `0` is a legitimate descriptor value and cannot double as "unset".
  Worth doing only if startup measures badly.

### Bounding the walk

The list gave "startup is bounded by construction" for free. The chain asserts
it instead: refuse a walk longer than `MAX_EDIT_CHUNKS` with
`StorageError::Corrupt`. One comparison per step, and it also stops a walk that
somehow re-enters a visited chunk from looping forever — though that is
defence in depth, since a `prev` that is wrong in the first place has to get
past its chunk's crc.

That reader-side invariant replacing a structural one is the honest cost of the
chain.

### What does not change

- **The allocator.** It does not know who allocated what, and gains no notion
  of a chain: chunk runs are allocated inside the window like everything else,
  and freed at compaction through the ordinary deferred-free path.
- **Cross-round state — there still is none.** A round records the descriptor
  changes *that round* made and nothing else; `prev` names where the previous
  round's record happens to sit, not what was in it. No round reads another
  round's chunk, and the dirty-tracking that
  [RFC 0046](0046-directory-deltas-in-the-window.md) avoided stays avoided.
- The window, its single write, its single barrier, the two-pass sizing, and
  `SlotEdit` itself.

### Compaction is unchanged

A snapshot is still a chunk whose `changed` covers every bucket; it simply
writes `prev = 0`. The frame names it as `head`, and every chunk before it is
freed through the existing deferred-free path, released when protection rolls
forward ([RFC 0047](0047-generational-journal-retirement.md)).

## What it costs

| | today ([0046](0046-directory-deltas-in-the-window.md)) | this |
|---|---|---|
| `Commit` frame | 25 + 8N bytes | **25 bytes** |
| journal bytes per compaction cycle | 4N² | **25N** |
| what caps the interval | the frame's own growth | startup reads only |
| startup reads | 1 + N | 1 + N (walk) or 2N (collect-then-apply) |
| recovery robustness | N known upfront from a crc'd frame | each `prev` crc'd by its own chunk, plus a length bound |
| RAM | the descriptor list | unchanged — the list stays resident |

## Why RFC 0046 rejected this, and what changed

[RFC 0046](0046-directory-deltas-in-the-window.md) chose the list and recorded
the chain as the escape hatch "if `edits` in the frame ever feels wrong — say
the threshold is raised far enough that the list is no longer trivial". That is
exactly the condition the table above describes, reached from the other
direction: the threshold *cannot* be raised while the list is there.

Its two objections:

- **"The order in which superseded chunks are freed cannot break a link."**
  This was the strong one, and it has since been answered by machinery that
  did not exist then. Every live chunk run is in `meta.runs()`, which
  `commit_journal` folds into `used` and protects at publish time; and
  [RFC 0047](0047-generational-journal-retirement.md) keeps **two** generations
  protected, so the chain a fallback recovery would walk survives until its
  frame is proven durable. Chunks are only freed at compaction, together, once
  the new frame references none of them.
- **"Startup is bounded by construction."** Traded for a reader-enforced bound,
  as above.

## Alternatives

### Raise `MAX_EDIT_CHUNKS` and keep the list

Free, and worth something on its own — but it walks straight into the quadratic.
The optimum for a 100 MiB snapshot is N ≈ 5120 at ~40 KB per checkpoint, an
order of magnitude short of what the chain reaches, and the tuning is
database-size-dependent in a way the chain removes entirely.

### Keep the list but delta-encode it in the frame

Carry only the entries added since the previous frame. Rejected: the frame stops
being self-contained — it now depends on the *previous frame*, which is the
retired journal's, i.e. exactly the dependency
[RFC 0046](0046-directory-deltas-in-the-window.md) was created to remove. A
chain puts that dependency in `data.bin`, which is not deleted.

### A root block indexing the chunk addresses

One block in `data.bin` holding the whole list, rewritten per checkpoint; the
frame names the block. O(1) frame and a single extra read at startup. Rejected:
it reintroduces a per-checkpoint write of the whole list — 4 KiB even when one
entry changed — which is the defect in a different place, and it is a block the
allocator must manage on top of the chunks it already does.

## What landed

`EditChunk` gained `prev`; `CommitFrame` lost `snapshot` and `edits` for a
single `head`. `edit::walk` follows the chain and returns it oldest-first, which
`load_commit` folds through the unchanged `Replay`. `MetaLog::frame()` became
`head()`, and `restored` takes the walked chain — the first chunk read as the
snapshot, which it is whenever a compaction has happened and which the
`COMPACT_FLOOR_BLOCKS` floor covers when it has not. `checkpoint::place_in`
passes `prev = if full { 0 } else { meta.head() }`, known before the carve and a
fixed 8 bytes, so the two-pass shape agreement is untouched.

`MetaLog` moved to its own `meta_log.rs`: `edit.rs` went 8 lines over the
350-line budget, and the seam was real — one module is a wire format plus a
fold, the other a retention policy.

Proven by `a_commit_frame_tracks_the_log_not_the_directory`, extended to
checkpoint eight more times and assert the journal length is **identical** every
round. Mutation-tested by putting the chunk list back in the frame beside the
head: the frame grows 65 → 73 bytes and the test fails. `the_frame_is_the_head_however_long_the_log`
walks a 41-chunk log asserting the head is always the newest and that a restored
log matches on head, runs and compaction state; `a_chain_that_never_snapshotted_restores_whole`
covers the young-database case where the chain's oldest chunk is a delta over an
empty directory. `compaction_bounds_the_log_and_the_chain_still_restores` (120
checkpoints, two types) was already the end-to-end guard and needed no change.

## Open questions

- **Where should the interval actually sit?** The chain makes the ratio rule
  (`edit_blocks >= snapshot_blocks`) the binding constraint, which is a sound
  default, but the count cap's new job — bounding startup — wants a number
  derived from measured read latency, not inherited from the frame-size era.
- **Does `prev` want a generation number?** A monotone counter per chunk would
  let recovery detect a *spliced* chain — one whose links are each individually
  valid but do not belong together, which a per-chunk crc cannot see — rather
  than only a corrupted one. Eight more bytes per chunk, inside the window, so
  free. Probably unnecessary: splicing needs a run to be freed and reused while
  still referenced, which protection forbids.
- **Does the walk belong in `edit.rs` or `commit.rs`?** `load_commit` already
  reads chunks; the walk is the only genuinely new reader logic and it may read
  better next to `Replay`.
