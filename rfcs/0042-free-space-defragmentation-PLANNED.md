# RFC 0042 — Free-space defragmentation

- **Status:** Planned (opened 2026-07-28)
- **Crates:** `wavedb-storage`
- **Code:** `crates/wavedb-storage/src/{alloc,directory_pages,settle}.rs`
- **Companion:** [RFC 0041](0041-single-barrier-checkpoint-WIP.md) — the
  single-barrier checkpoint that consumes the windows this produces

## Summary

An async background process that **relocates isolated live runs so free extents
coalesce into windows large enough to hold a whole checkpoint**. The checkpoint
itself needs no knowledge of it: it asks `BlockAllocator::alloc(total)` for one
contiguous run, and best-fit lands in a recycled window whenever one exists. The
defragmenter's only job is to keep such windows existing.

## Motivation

Copy-on-write page rewrites free the old run and allocate elsewhere, so
`data.bin` accumulates holes whose *total* size is ample but whose *largest
contiguous* extent shrinks. Under [RFC 0041](0041-single-barrier-checkpoint-WIP.md)
a checkpoint asks for one run sized to the whole touched set; once no hole fits,
every checkpoint grows the tail and the file grows monotonically even though most
of it is free.

The resource being defended is **IOps, not disk space**. Spending reads and
writes in the background to keep one large window available is worth it precisely
because it converts each future checkpoint from "grow the tail" into "reuse a
window", and keeps the file bounded without ever putting a compaction on the
serving path.

## Design sketch

- **Selection uses state the allocator already keeps.** `by_pos` (start → count)
  makes a live run's neighbourhood visible: a run with free extents on both sides
  is the cheapest relocation target, because moving it merges three extents into
  one. `by_size` ranks the result. `free_blocks` / `free_extent_count` are already
  exposed as the fragmentation gauge.
- **The heuristic is coalesced-bytes per byte moved.** Prefer the relocation that
  yields the largest contiguous extent for the least data copied — small live runs
  stranded between big holes first.
- **Relocation reuses the RFC 0041 write path.** Plan the moved pages in RAM,
  allocate one window, one `write_run`, one `sync`, swap the descriptors under
  `StructStorage::directory()`, emit the `Commit` frame. A move is *only* a
  change of address: page bytes, `STRUCT_HASH` routing and bucket membership are
  untouched, so no index above the block layer observes it.
- **Trigger** on a fragmentation gauge — largest free extent versus the recent
  checkpoint window size — not on a fixed schedule: the question is always "would
  the next checkpoint fit a hole?".
- **Budget and backpressure.** The cleaner runs with an IOps budget per interval
  and yields to serving; it is a maintenance task like `drain` / `commit_journal`
  in `wavedb-quick-node`'s `maintain` loop, and an engine fault stops it without
  touching acked writes.
- **Tail truncation** stays `BlockAllocator::truncate` — after consolidation a
  free tail is genuinely reclaimable.

## Open questions

- Trigger thresholds, and whether the gauge is absolute (largest extent in blocks)
  or relative (largest extent ÷ recent window size).
- Whether relocation rides an ordinary checkpoint's window or always takes its
  own — one write either way, but sharing changes the RAM peak.
- Interaction with unbounded history growth (no cold tier —
  [RFC 0033](0033-cold-history-slow-node-tier-DEPRECATED.md)): archived versions
  are cold and never rewritten, so they are the ideal relocation filler, but they
  also mean live-data density falls over time.
- Whether the defragmenter should ever *split* a request across holes instead of
  moving data (cheaper, but reopens the "one run or a few" question in
  [RFC 0041](0041-single-barrier-checkpoint-WIP.md)).
