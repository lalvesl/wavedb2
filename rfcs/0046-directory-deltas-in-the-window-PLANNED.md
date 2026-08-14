# RFC 0046 — Directory deltas in the settle window

- **Status:** Planned — opened 2026-07-28
- **Amends:** [RFC 0043](0043-descriptors-in-the-commit-frame.md) (which put the
  *whole* addressing state in every `Commit` frame)
- **Builds on:** [RFC 0041](0041-single-barrier-checkpoint.md) (the one
  contiguous window a settle already writes — this rides it)
- **Crates:** `wavedb-storage`
- **Code (target):** `crates/wavedb-storage/src/{checkpoint,plan,commit,journal}.rs`

## Summary

A settle window stops being only pages and becomes **self-describing**: its last
blocks carry the `BlockDescriptor` changes those pages caused. The metadata
therefore costs **zero additional IOps** — it is bytes inside a positioned write
that was already happening, made durable by an `fsync` that was already
happening. The `Commit` frame shrinks from "every bucket of every type" to a
short list of the edit chunks written since the last full snapshot.

A checkpoint's metadata cost stops scaling with the size of the database and
starts scaling with the size of the change.

## The problem

[RFC 0043](0043-descriptors-in-the-commit-frame.md) made a checkpoint's
metadata one journal IOp, which was right, but it made that IOp carry every
registered type's whole `Vec<BlockDescriptor>` — 8 bytes per bucket, every
bucket, every checkpoint. That volume is a function of the database, not of the
work:

| `data.bin` | buckets (≈16 KiB pages) | `Commit` frame, **per checkpoint** |
|---|---|---|
| 2 GiB | 131 072 | 1 MiB |
| 20 GiB | 1 310 720 | 10 MiB |
| 200 GiB | 13 107 200 | 100 MiB |

Beside the work it certifies, the amplification is the whole story. A checkpoint
that settles 100 pages writes ~1.6 MiB of pages either way; at 2 GiB it adds
1 MiB of frame (metadata ≈ 40 % of the checkpoint), at 200 GiB it adds 100 MiB
(≈ 98 %). Every type is written every time, including the ones untouched since
the database was created.

The engine's scarce resources are RAM and disk IOps. This spends IOps
proportional to the *stock* to record a change in the *flow* — the last place in
the write path that still does, now that the directory chain is gone.

### Why the frame has to be self-sufficient today

One line causes it: the retired journal is deleted immediately after the
`Commit` frame lands, so the newest frame is the only surviving mention of the
addressing state. The fix is not to shrink the frame but to give the state a
home that is **not** deleted every checkpoint — and `data.bin` already is one.

## Design

### The window becomes self-describing

[RFC 0041](0041-single-barrier-checkpoint.md) already carves one contiguous
window per settle round and fills it with page images and a grown dictionary.
Add one more target:

```text
window = [ page ][ page ][ … ][ dict ][ edit chunk ]
```

The **edit chunk** is a crc-framed record of exactly what this round changed:

```rust
pub struct EditChunk {
    /// Per type: only the buckets whose descriptor moved, plus the bucket
    /// count after the round (so a linear-hashing split needs no record of
    /// its own).
    pub slots: Vec<SlotEdit>,
    /// (STRUCT_HASH, dictionary run descriptor) — only if it grew.
    pub dicts: Vec<(u64, u64)>,
}

pub struct SlotEdit {
    pub struct_hash: u64,
    pub buckets: u32,
    pub changed: Vec<(u32, u64)>, // (bucket, raw BlockDescriptor)
}
```

This is one more `Target` in `targets_of`, one more entry in `carve`, one more
`copy_from_slice` in `assemble`. **The same `write_run`, the same `fsync`.** A
round that changes 500 buckets adds ~6 KiB to a window that was already several
MiB — and it stays ~6 KiB when the database is 200 GiB.

Sizing is known in time: the number of changed buckets is `targets_of`'s page
count plus any buckets `plan_splits` appended, all decided before `carve`; the
chunk is serialised after `install` has set the new descriptors, into space the
carve already reserved.

**No dirty-tracking state is needed anywhere.** A round records the changes that
round made, in the window that made them. The `Directory` gains nothing, and the
RAM cost of the whole mechanism is the chunk buffer itself, alive for the
duration of one write.

### The frame carries the list, not a chain

The chunks need to be findable. The obvious shape is a back-pointer in each
chunk to the previous one, walked at startup back to the snapshot. The cheaper
shape is to skip the pointers: the `Commit` frame already exists, is already
appended once per checkpoint, and is now nearly empty.

```rust
pub struct CommitFrame {
    /// The retired journal's timestamp — this frame IS its DONE marker.
    pub journal_ts: u64,
    /// The full-state snapshot's run (the "princípio" the deltas patch).
    pub snapshot: u64,
    /// Every edit chunk written since that snapshot, oldest first.
    pub edits: Vec<u64>,
}
```

`edits` is O(settle rounds since the last snapshot), not O(database) — bounded
by the compaction threshold, so a few hundred bytes. Dropping the back-pointers
buys two things: a chunk is pure payload (nothing points at anything, so the
order in which superseded chunks are freed cannot break a link), and startup is
bounded by construction instead of by "however far back the chain goes".

Cost of the frame's own append: unchanged, one small crc-framed append + fsync,
the same barrier today's checkpoint already pays.

### Compaction

When `edits` grows past its threshold, the next settle round emits, alongside
its pages, a **full snapshot image** of every type's descriptor vector — from
the directories already resident in RAM. The frame then names the new snapshot
and an empty `edits`; the old snapshot run and every superseded chunk are freed
through the allocator's existing deferred-free path, releasing when protection
rolls forward at the following commit.

So yes — as you put it, the reclamation is a deallocation rather than an
`unlink`. It is also the only part of this that is not free, and it is
accepted: at the proposed threshold (edit bytes ≳ snapshot bytes) roughly one
round in N pays it, the snapshot is a sequential image of a vector already in
RAM, and it rides the same window write as everything else.

### Recovery

1. Read the newest decodable `Commit` frame — unchanged, `journal.rs` already
   does this.
2. Read `snapshot` (one contiguous run) → install the directories.
3. Read each run in `edits`, oldest first, applying `SlotEdit`s → the current
   directories. A chunk whose crc fails is a corrupt database, exactly like a
   corrupt page (the frame naming it is the assertion that it is durable).
4. Derive the allocator from the resulting descriptors **plus** the snapshot and
   chunk runs — the loop `load_commit` already runs, with two more entries.
5. Journals with `ts <= journal_ts` are covered — delete them; the rest replay.

Reads: 1 + `edits.len()`, scattered, bounded by the compaction threshold. That
is more read IOps at startup than today's single frame; startup is the one place
this design deliberately spends, and it is the right place to spend.

### What it costs

| | today ([0043](0043-descriptors-in-the-commit-frame.md)) | this |
|---|---|---|
| metadata bytes per checkpoint | every bucket of every type | only changed buckets |
| …at 2 GiB / 131 072 buckets | 1 MiB | a few KiB |
| …at 200 GiB | ~100 MiB | a few KiB |
| **extra write IOps for metadata** | 1 (the big journal append) | **0** — rides the window |
| barriers per checkpoint | 2 | 2 |
| extra RAM | — | one chunk buffer, per round |
| startup reads | 1 | 1 + chunks since snapshot |
| files in the data directory | 2 kinds | 2 kinds |

The honest cost, beyond startup: the snapshot and the chunks become **allocator
tenants**. They are short-lived runs interleaved among long-lived page runs, so
freeing them at compaction leaves holes — the pattern
[RFC 0042](0042-free-space-defragmentation.md) exists to clean up. Given that
disk space is not the scarce resource and the defragmenter is already running,
this is a fair price for a metadata path that costs no IOps at all.

## Alternatives

### A separate append-only manifest file

`manifest_<ts>.log`: a `Snapshot` frame per generation plus one small `Edit`
appended per checkpoint, compacted by writing a new generation and unlinking the
old. It keeps the metadata log out of the allocator entirely, so it never
fragments `data.bin` and compaction is `write new; fsync; unlink old` with no
protected-set interaction at all.

**Rejected — the IOp accounting is identical and it gives up the free ride.**
Both designs are 2 writes and 2 barriers per checkpoint (window + `fsync`, then
one small append + `fsync` to either the manifest or the journal). But the
manifest's append is a *new* write that has to happen, where an edit chunk is
bytes appended to a buffer that is written anyway. The manifest also costs a
third file, its own framing, its own rotation and its own recovery root — and it
breaks the property that `data.bin` plus a journal is a complete database, which
matters for copying and backing one up. The allocator churn it avoids is real
but small, bounded, and already handled.

### Two-level directory: group blocks in `data.bin`, roots in the frame

Persist descriptors as 4 KiB group blocks (~508 buckets each) and keep only the
group addresses in the frame; rewrite dirty groups. Rejected: the frame is still
O(directory) (8 KiB per checkpoint at 2 GiB, 800 KiB at 200 GiB — better, not
fixed), and one changed bucket still rewrites a whole 4 KiB group.

### Keep the deltas in the journal and retain journals until a snapshot

Then the journal *is* the delta log and nothing new exists. Rejected: journals
carry record batches, so retaining them to preserve a few KiB of addressing
deltas retains megabytes of redo — and releasing exactly those bytes is what a
checkpoint is for.

### Implicit placement — no directory to persist

Bucket `N` always at block `f(N)`, or double-buffered across two fixed slots with
one bit per bucket saying which is live. Dissolves the problem entirely, and
gives up copy-on-write: in-place page writes need torn-page recovery, and fixed
addresses destroy the contiguous-window property
[RFC 0041](0041-single-barrier-checkpoint.md) is built on. An architecture
change, not an optimisation.

## Open questions

- **The one-barrier checkpoint.** Once the edit chunk is durable inside the
  window's own `fsync`, the `Commit` frame is only a *pointer* — so it could ride
  the next ordinary `Batch` append instead of taking a barrier of its own,
  making a checkpoint cost **one** dedicated barrier instead of two. The price
  is that the retired journal cannot be deleted until that next append lands,
  so journals linger a little. This was 0041's original intent, dropped then
  because the frame carried the state itself; carrying only a pointer brings it
  back within reach. Worth doing as a follow-up, not in the first slice.
- **Compaction threshold.** Edit bytes ≳ snapshot bytes is self-scaling and the
  proposed default; a small database probably wants a floor (`max(1 MiB, …)`).
  Wants a measurement.
- **Back-pointers after all?** If `edits` in the frame ever feels wrong — say
  the threshold is raised far enough that the list is no longer trivial — a
  back-pointer per chunk restores O(1) frame size at the cost of ordering
  constraints on frees. The list is the better default; the chain is the escape
  hatch.
- **A whole-type edit variant.** A type whose buckets nearly all moved — a large
  defrag round, a rehash storm — is cheaper as a full descriptor vector than as
  a `changed` list. Letting `SlotEdit` carry either is a small addition; worth
  doing only if defrag rounds measure badly.
- **Defrag and chunk size.** Every relocation is a changed descriptor, so
  [RFC 0042](0042-free-space-defragmentation.md)'s budget now also bounds how
  large an edit chunk gets. Same knob, second effect — worth stating there.
