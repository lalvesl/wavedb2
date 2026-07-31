# RFC 0019 — Journal-rooted recovery

- **Status:** Implemented (landed 2026-07-07 — the journal work J1–J5)
- **Supersedes:** [RFC 0030 — superblock-pointer checkpoint](0030-superblock-pointer-checkpoint-DEPRECATED.md)
- **Crates:** `wavedb-storage`
- **Code:** journal + WAL paths in `crates/wavedb-storage/src/`

## Summary

Durability is **journal-first WAL**: append to the journal + commit to the cache
under the journal lock is *the* atomic unit. Recovery **roots in the newest valid
`Commit` frame** — a single frame naming the roots of every type — so open replays
only the tail, not all of history, and `data.bin` is an authoritative checkpoint
rather than something rebuilt from scratch each boot.

## Motivation

The interim model truncated `data.bin` to its superblock on every open and
replayed the **entire** journal through the live commit+settle path. Correct, but
the journal grew unbounded and startup was O(history). The goal: make `data.bin`
a real checkpoint so the journal truncates and open replays only the tail.

An earlier checkpoint attempt used a superblock pointer
([RFC 0030](0030-superblock-pointer-checkpoint-DEPRECATED.md)); it was superseded
the same day by rooting recovery in the journal itself.

## Design

- **The atomic unit is journal-append + cache-commit under the journal lock.**
  A batch is durable once its frame is in the journal; the cache commit rides the
  same lock.
- **Timestamped journal rotation.** `journal_<ts>.log` rotates with **no write
  lock**; directory chains are written as copy-on-write blocks in `data.bin`.
- **One atomic `Commit` frame.** A checkpoint writes ONE `Commit` frame — the
  roots of all types — into the *new* journal, retiring the old one. The
  superblock is write-once again (no mutable pointer).
- **Recovery roots in the newest valid `Commit`.** Open finds the latest good
  `Commit`, loads that projection, and replays only what follows — startup is
  O(tail), not O(history). The journal no longer grows unbounded.
- **Background maintenance.** A node task drains the pending queue → threshold
  commit → cache eviction to a budget; settle is deferred behind a `pending`
  queue with unsettled-remove tombstones, and reads are page-backed so the cache
  is genuinely a cache ([RFC 0018](0018-storage-engine.md)).

## Interaction with the history model

Recovery is **physical** (blocks, frames, roots); the *logical* history — which
version supersedes which — lives entirely in the anchor/`Succession` model above
the batch ([RFC 0009](0009-anchors-succession-and-history.md)). This separation
is exactly why the journal cursor could **not** serve as the sync log: rotated
journals are deleted and `Batch` frames are physical, not logical
([RFC 0028](0028-journal-commit-cursor-sync-DEPRECATED.md)).

## Alternatives

- **Superblock-pointer checkpoint (S2/S3)** —
  [RFC 0030](0030-superblock-pointer-checkpoint-DEPRECATED.md), superseded by the
  journal-rooted `Commit` (a mutable superblock pointer reintroduced a
  write-in-place hazard the write-once superblock avoids).
