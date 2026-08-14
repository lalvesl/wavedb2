# RFC 0047 — Generational journal retirement

- **Status:** Implemented (landed 2026-07-29)
- **Builds on:** [RFC 0046](0046-directory-deltas-in-the-window.md) (which made
  the `Commit` frame a deferred, barrier-free append and introduced
  `crate::retire`)
- **Crates:** `wavedb-storage` (`wavedb-quick-node`'s maintenance policy is the
  only affected caller)
- **Code:** `crates/wavedb-storage/src/{retire,commit,apply,alloc}.rs`

## Summary

A retired journal is deleted **at the next checkpoint** — the moment the
*current* journal itself becomes old — instead of at the first opportunity after
its `Commit` frame becomes durable. The deletion and the protected-set roll it
comes with move off the write path entirely and become one more step of the
checkpoint that supersedes them.

A journal generation is cheap: it is a file on disk, and disk space is not the
scarce resource. What retiring it *early* costs is a barrier on an idle node and
a lock plus an `unlink` inside a latency-sensitive write. This RFC stops paying
those.

## Motivation

[RFC 0046](0046-directory-deltas-in-the-window.md) got the checkpoint itself to
one barrier by writing the `Commit` frame unsynced. But the two things that
frame *authorises* — deleting the retired journal, rolling the allocator's
protected set — still had to wait for it to be durable, and the design chased
that durability as soon as it could:

| where | what it does today | cost |
|---|---|---|
| `apply_inner`, after every batch fsync | `finish_retirement()` | a lock take per write; an `unlink` syscall inside the **first** write after each checkpoint |
| the quick-node maintenance tick, when nothing is pending | `force_retirement()` | **a full journal `fsync`** — a barrier paid purely for housekeeping |
| the top of `commit_journal` | `force_retirement()` | another barrier, whenever the previous frame never got carried |

So the accounting that RFC 0046 claims — one barrier per checkpoint — is true of
the checkpoint and not quite true of the system. A bursty node (writes, then a
checkpoint, then quiet) pays a second barrier within one maintenance period, on
behalf of that checkpoint, just later and from a different call site.

The reason for the hurry was never stated as a requirement. It is housekeeping:
delete a file, release some deferred frees. Neither is urgent, and the engine
already has a natural moment for both — the next rotation, when the journal
holding the frame stops being the live one.

## Design

### The rule

At most **two** journals are retained: the current one, and the one retired by
the previous checkpoint. `commit_journal` opens by disposing of the older
generation, and the pending retirement it leaves behind is disposed of by the
checkpoint after it.

```text
C_k     : retires J_k, writes frame F_k (unsynced) into J_{k+1}
          → pending: delete J_k, set_protected(S_k)
… epoch k: writes append to J_{k+1}; settle rounds write windows.
           NOTHING touches the pending retirement.
C_{k+1} : rotate J_{k+1} → J_{k+2}
          → F_k is durable ⇒ delete J_k, set_protected(S_k)   ← the whole change
          → drain, window write, fsync data.bin
          → write F_{k+1} into J_{k+2}; pending: delete J_{k+1}, S_{k+1}
```

The checkpoint's own accounting is unchanged: one write, one barrier. The
disposal step adds one `unlink` and one in-memory set swap.

### Knowing the frame is durable, without syncing to find out

`F_k` lives in `J_{k+1}`, and `J_{k+1}` is exactly the journal `C_{k+1}` has in
hand the instant it rotates. `Journal` already counts its own barriers
(`barriers()`, added by RFC 0046 for the tests). So:

- when the deferred frame is appended, record the journal's barrier count in the
  same critical section that appends it — call it `frame_barrier`;
- at the next rotation, `old.barriers() > frame_barrier` ⇒ every byte written
  before that count, the frame included, has been flushed by an ordinary
  `Batch` append. Delete and roll, for free.

Reading the count **after** the append and under the same guard is load-bearing:
`commit_journal` rotates before it appends, so a concurrent writer can fsync a
batch into the new journal in between. A bare `barriers() > 0` would read that
sync as proof of a frame that had not been written yet.

In practice the test always passes, for a structural reason: a checkpoint fires
because the journal grew past a threshold, growth means `Batch` appends, and
every `Batch` append fsyncs. The only way to reach a rotation with no barrier
since the frame is to call `commit_journal` twice with no write in between — a
checkpoint whose drain has nothing to do.

That case takes the fallback: `old.sync()` — the retired journal is the file
holding the frame — then dispose. One barrier, in the case where the checkpoint
had no work anyway. The alternative (carry a second pending generation) is
rejected below.

### What stays

`force_retirement()` keeps its shape and loses two of its three callers. It
remains the explicit "no write is coming, close this out now" call, used by
**graceful shutdown** — where the engine is about to be dropped and there is no
next checkpoint. The idle maintenance tick stops calling it; a retained journal
on an idle node is now the expected steady state, not a leak.

`apply_inner` loses its trailing `finish_retirement()`. The write path ends at
the cache commit again.

Recovery is untouched. It already tolerates a retained-but-covered journal:
`restore` skips and deletes every journal with `ts <= commit.journal_ts`
(`covered_journal_left_behind_is_skipped_and_cleaned`). This RFC makes that path
ordinary rather than exceptional, and raises its bound from one such file to two.

### Block protection becomes two generations

Deferring the disposal exposed something the eager version had been hiding.
`BlockAllocator` protected exactly one set of runs — the last checkpoint's —
and that set was installed **by the retirement**, i.e. by the first write after
the checkpoint. So between a checkpoint publishing and the next write landing,
protection still named the *previous* checkpoint's runs, and a settle or defrag
round in that gap would free a run the newest frame names as if it were
ordinary garbage. Holding the retirement for a whole epoch would have widened
that gap from "until the next write" to "until the next checkpoint".

The cause is that "protect what a crash reopens into" was conflated with "one
checkpoint". While a frame is unproven there are genuinely **two** reachable
roots — the new frame and the one before it — so both sets must be protected:

- `commit_journal` calls `set_protected(S_k)` as it publishes, under the
  allocator guard it already holds, before anything can free into that state;
- `set_protected` demotes the set it replaces to `previous` instead of dropping
  it, and `is_protected` probes both (each set alone is a snapshot of live
  allocations, so it never self-overlaps and one probe per set still decides);
- `release_previous()` — called by `dispose`, once the next checkpoint has
  proven the frame durable — drops the older set and releases the frees it
  alone was holding.

`Retiring` no longer carries a protected set at all: protection is installed at
publish time, and all the retirement has to do is lift the older half.

### Crash model

Unchanged in kind, one generation deeper. A crash during epoch `k` finds `F_k`
durable or not:

- **durable** — recovery roots at `F_k`, deletes `J_k` (covered) and replays
  `J_{k+1}`'s batches. Exactly today's outcome.
- **not durable** (no write since the checkpoint) — recovery roots at
  `F_{k-1}`, and replays `J_k` *and* `J_{k+1}`. That is why `J_k` is still on
  disk, and why protection is still `S_{k-1}`: both halves of the pending
  retirement are held together, so the fallback is consistent.

Nothing acked is at risk in either case — an acked write is a journaled write,
and both journals are present. The cost of the second case is re-settling one
extra journal's batches, which is idempotent by construction (settle writes
cache state).

## What it costs

| | today ([0046](0046-directory-deltas-in-the-window.md)) | this |
|---|---|---|
| barriers per checkpoint, **including housekeeping** | 1 + 1 whenever no write carries the frame | **1** |
| retirement work on the write path | a lock take per batch; an `unlink` in the first write after a checkpoint | **none** |
| retained journals (steady state) | 1 | 2 |
| retained journals (after a crash) | ≤ 2 | ≤ 3 |
| deferred frees held | one checkpoint epoch | **two** |
| startup replay after a crash | ≤ 1 extra journal | ≤ 2 extra journals |

Two costs are real and both are paid in the resource this project treats as
abundant:

- **Disk.** One more journal file — bounded by `checkpoint_after_bytes`, 64 MiB
  by default — plus one extra generation of deferred frees inside `data.bin`,
  bounded by one checkpoint's worth of superseded runs.
- **Startup.** `restore` opens and replays every journal it finds before
  deciding which are covered, so a retained generation is read and crc-verified
  at open. Bounded by the same 64 MiB, and startup is already where this design
  spends (RFC 0046).

## Alternatives

### Carry the retirement forward instead of syncing (unbounded generations)

Rather than the `old.sync()` fallback, let a not-yet-durable frame push its
retirement into a queue and drain it whenever durability is observed. Strictly
fewer barriers. **Rejected:** it trades a barrier that is only ever paid by a
no-op checkpoint for an unbounded retention list, and "at most two journals" is
worth more as an invariant than that barrier is worth as a saving. The escape
hatch stays available if the fallback ever measures.

### Keep completing retirements on the write path, drop only the idle force

Half the change: leave `apply_inner`'s `finish_retirement()`, stop the
maintenance tick from forcing. **Rejected:** it keeps a lock take on every write
and an `unlink` inside one of them, to buy back a file that costs nothing to
keep. If the deletion is not urgent, the write path should not be the one doing
it.

### Sync the journal at rotation, unconditionally

Delete the barrier-mark bookkeeping by always making the outgoing journal
durable as it retires. **Rejected:** that is the barrier this RFC exists to
remove, reintroduced at a fixed point instead of an occasional one.

### Delete the journal eagerly and make the frame self-sufficient again

The pre-0046 shape (a frame carrying the whole addressing state) needs no
retention at all. **Rejected** by RFC 0046 on its own terms: metadata that
scales with the database rather than the change.

## What landed

`alloc.rs` gained the second generation: a `previous` map that `set_protected`
demotes into and `release_previous` clears, `is_protected` probing both through
a shared `covers`, and a `deferred_blocks()` gauge so a test can see protection
engage. `commit.rs` calls `set_protected(&used)` at publish, under the guard it
already held.

`retire.rs` keeps `Retiring` (now carrying `frame_barrier`), `force_retirement`
and `is_retiring`, and replaces `finish_retirement` with two pieces: a private
`dispose` (roll protection, unlink) and `retire_previous(pending, carrier)`,
called by `commit_journal` at its new step 2 with the journal it just rotated
out. `commit_journal` claims the pending record **before** rotating — between
the rotation and step 2 the frame's carrier is not `self.journal`, so a
concurrent `force_retirement` observing the record there would sync the wrong
file and delete a journal whose frame is not durable. `apply_inner` ends at the
cache commit again; the quick-node maintenance tick loses its idle-force branch
and the `checkpointed` flag that existed only to keep it from firing too early.

The simplification is in the shape, not the line count (docs and a new test grew
it): three call sites that could complete a retirement became one, and the
lock-ordering hazard that came with them is gone by construction. `retiring` was
"the one lock taken on both sides of `alloc`" — a checkpoint publishing under it
while a write completed it in the opposite order, held apart only by a comment
warning not to fold two statements into an `if let`. Now it is never held across
another lock at all, so it can join no cycle.

Proven by: `a_checkpoint_is_one_write_and_one_barrier`, extended past the frame
to the next checkpoint — a batch's fsync carries the frame but does **not**
dispose, and the following checkpoint is again one write and one barrier with
none taken on the fresh journal; and
`a_checkpoint_with_no_writes_syncs_the_carrier_before_disposing` for the
fallback, which reads the barrier count off the retained carrier (no batch
touched that file, so the count is the fallback's own). The protection change is
held by `a_settle_after_a_checkpoint_cannot_reuse_its_runs` (rewriting the
records a checkpoint just named must leave blocks *deferred*, not pooled) and by
`protected_frees_defer_until_released`, extended to assert that a demoted set
still protects and only `release_previous` retires it. Both were checked by
mutation — dropping the publish-time `set_protected`, and making `set_protected`
forget the older generation — and each fails its test.
`commit_retires_the_old_journal_and_reopen_is_cold`,
`a_deferred_commit_that_never_lands_replays_the_retained_journal`,
`torn_commit_frame_falls_back_to_the_old_journal` and
`covered_journal_left_behind_is_skipped_and_cleaned` are unchanged and still
green — the recovery contract did not move.

## Open questions

- **Should recovery avoid replaying covered journals at all?** `restore` reads
  every journal before it knows which are covered, because the newest `Commit`
  is in the newest file. Scanning newest-first to find `journal_ts`, then
  skipping the covered ones without decoding, would make a retained generation
  cost an `unlink` instead of a full read. Independent of this RFC, but this is
  what makes it worth doing.
- **Is `checkpoint_after_bytes` still the right single knob?** It now also sets
  the retained-journal ceiling and the crash-replay ceiling — a third meaning on
  top of the two [RFC 0041](0041-single-barrier-checkpoint.md) already gives it.
- **Should shutdown force at all?** `force_retirement()` at shutdown pays a
  barrier to leave the directory tidy. Skipping it is safe — the next open
  simply recovers — and the process is exiting anyway. Kept for now because a
  clean shutdown that replays nothing is a property worth having.
