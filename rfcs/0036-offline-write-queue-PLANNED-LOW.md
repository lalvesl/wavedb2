# RFC 0036 — W8: Offline write queue

- **Status:** Planned (low) — slice 1 (Unique offline) shipped; the remainder
  is deferred, low priority. The near-term focus is online + small-window
  offline, which the shipped slice already covers.
- **Crates:** `wavedb`, `wavedb-net`
- **Depends on:** [RFC 0024](0024-client-db-and-cache.md),
  [RFC 0021](0021-connection-manager.md),
  [RFC 0022](0022-live-sync-navigation-catchup.md),
  [RFC 0009](0009-anchors-succession-and-history.md)

> **Progress — slice 1 landed (2026-07-24): Unique saves, in-memory queue.**
> `Db::save_unique` no longer refuses offline: on an `Error::Transport` with a
> cache attached it appends a `QueuedOp{tenant, struct_hash, command, payload}`
> to an `OfflineQueue` (`wavedb::offline_queue`), mirrors the value locally, and
> returns `Ok` (provisional). A `save` first drains the queue **node-first,
> FIFO** (`Db::drain_offline_queue`, also public for an app to force a sync):
> each op replays through the ordinary command path; a transport fault stops the
> drain (keep the rest), any authoritative answer — success **or** a
> `Conflict`/refusal — drops the op (never a silent overwrite; the live-sync
> catch-up reconciles the cache). Proven by `offline_queue_e2e` (offline save →
> reconnect → drain → a cache-less handle confirms the node has it) plus unit
> tests (FIFO, the stop/drop rule).
>
> **Open questions, resolved for this slice.**
> - *Conflict surface* → **policy decided, surface not yet built.** WaveDB
>   deliberately does **not** auto-merge concurrent edits to data shared across
>   users — few data types admit a generic auto-merge (git leans on explicit
>   merge-conflicts for exactly this), so resolution is the **developer's** job.
>   The target: a replayed write that loses its `Expect` race **surfaces
>   `Error::Conflict` and refuses**, leaving the app to resolve. This slice's
>   placeholder is a silent **node-first drop** (catch-up reconciles the cache);
>   the app-facing surface is a later phase. **Single-tenant data — the
>   near-term focus — effectively never conflicts, so the placeholder holds.**
> - *Queue durability* → **in memory** — it survives a network blip in one
>   process (the case write-through loses today), not a process restart. A
>   durable on-store queue is a later phase (the native `PageStore` cache is
>   per-registered-type, so a reserved-hash record does not route — it needs its
>   own sidecar, unlike the flat wasm `IdbStore`).
> - *Drain trigger/layer* → the drain lives in **`Db`**, not the connection
>   manager the Summary names: the manager is `wavedb-net` and cannot see the
>   `wavedb` cache/queue. It fires on the next successful command (node-first).
>
> **Remaining (deferred, low priority).** NonUnique `insert`/`update`/`remove`,
> a durable queue across restarts, and the app-facing conflict surface.
> **Correction:** NonUnique offline is *not* a provisional-id/reconciliation
> problem. A plain NonUnique id is `mint_floored_id` = `key_nanos()` (time +
> process counter), **unique per tenant** and mintable client-side; today the
> node mints it and the cache adopts, but offline the client can mint locally
> and the node honours the supplied id on replay — no swap. A
> `#[wavedb::key]` id is `keyed_id` = a **deterministic content hash**, already
> client-computable; its only open question is *natural-key uniqueness offline*,
> which is the **developer's** guarantee — if they cannot ensure it, the app
> gates the write (`if offline { skip the NonUnique insert }`), exactly as
> WaveDB leaves conflict resolution to the developer (see the resolved
> conflict-surface open question below).

## Summary

Turn M6's **refused** offline writes into a **durable local queue** that replays
through the reconnect cursor path when the node comes back — order preserved,
node-first semantics intact (the queue drains before reads trust the node).

## Motivation

Today the write-through cache is strictly behind the node: offline writes
**refuse** on purpose, so the cache can never diverge
([RFC 0024](0024-client-db-and-cache.md)). That is the right default while there
is no reconciliation story, but it means an app loses work the moment the network
blips. W8 adds the missing piece — a queue that lets an offline write *succeed
locally* and reach the node later — **without** reintroducing divergence.

## Design (target)

- **Durable queue in the local store.** An offline write is appended to a queue
  (in the same cache backend, [RFC 0025](0025-wasm-indexeddb-target.md) /
  `PageStore`) instead of refusing; the typed op returns as if provisionally
  applied.
- **Replay on reconnect, in order.** The connection manager
  ([RFC 0021](0021-connection-manager.md)) — the owner of reconnect — drains the
  queue in FIFO order through the ordinary command path before it lets reads
  trust the node again, preserving node-first ordering.
- **Conflicts are honest.** A queued write that races a newer node version
  surfaces as a typed `Error::Conflict` via the `Expect` guard
  ([RFC 0009](0009-anchors-succession-and-history.md)) — never a silent
  overwrite. The app decides retry/merge.
- **Cursor coherence.** Replay interleaves with catch-up
  ([RFC 0022](0022-live-sync-navigation-catchup.md)) so the cache converges to
  the node's authoritative state after the drain.

## Open questions

- ~~The conflict-resolution surface (retry vs surface-to-app vs last-writer).~~
  **Resolved: surface-to-app.** WaveDB never auto-merges concurrent edits to
  shared data — a replayed write that loses its `Expect` race surfaces
  `Error::Conflict` and refuses; the developer resolves it (git-style
  merge-conflict handling is the analogy — no engine can own this generically).
  Building the app-facing surface is deferred, and multi-user shared-offline
  reconciliation stays out of scope; the near-term focus is **online + small
  offline windows**, where single-tenant data effectively never conflicts.
- Whether the queue is best-effort (drop on cache clear) or a first-class durable
  log with its own recovery.

## Relationship to non-goals

This is the first real step toward offline-first, which the project lists as a
*current* non-goal ([RFC 0001](0001-vision-and-non-goals.md)) — W8 is the
*write-queue* slice only, not full offline-first reconciliation, which stays
deferred.
