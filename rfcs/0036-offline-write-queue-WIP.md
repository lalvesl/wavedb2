# RFC 0036 — W8: Offline write queue

- **Status:** Planned (M7 W8 — the last M7 piece)
- **Crates:** `wavedb`, `wavedb-net`
- **Depends on:** [RFC 0024](0024-client-db-and-cache.md),
  [RFC 0021](0021-connection-manager.md),
  [RFC 0022](0022-live-sync-navigation-catchup.md),
  [RFC 0009](0009-anchors-succession-and-history.md)

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

- The conflict-resolution surface (retry vs surface-to-app vs last-writer) —
  needs a policy, likely per-type or per-op.
- Whether the queue is best-effort (drop on cache clear) or a first-class durable
  log with its own recovery.

## Relationship to non-goals

This is the first real step toward offline-first, which the project lists as a
*current* non-goal ([RFC 0001](0001-vision-and-non-goals.md)) — W8 is the
*write-queue* slice only, not full offline-first reconciliation, which stays
deferred.
