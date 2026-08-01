# RFC 0022 — Live sync by navigation catch-up

- **Status:** Implemented (poll path + navigation, and WS reconnect catch-up via
  [RFC 0034](0034-ws-reconnect-catchup.md), landed 2026-07-24)
- **Supersedes:** [RFC 0028 — journal commit-cursor sync](0028-journal-commit-cursor-sync-DEPRECATED.md),
  [RFC 0029 — Bloom-filter screen-sync](0029-bloom-filter-screen-sync-DEPRECATED.md),
  [RFC 0032 — node-side poll buffer](0032-node-side-poll-buffer-DEPRECATED.md)
- **Crates:** `wavedb-net`, `wavedb-core`, `wavedb-quick-node`, `wavedb`
- **Code:** `core::expose_changes`; `net::sync`; `Db::watch_*`

## Summary

Live sync is **declared subscriptions + navigation catch-up**. A client declares
interest in a *topic* — a Unique anchor or one collection `Pivot` — and receives
each matching mutation (pushed over WebSocket, or delivered by an HTTP poll).
Catch-up after an outage is **stateless**: each topic carries an instant
**cursor**, and the node answers by *navigating the data itself* — the
recency/dead logs for a collection, the version chain for a Unique record. The
node keeps **zero per-session state**; an outage or restart loses nothing.

## Motivation

A reconnecting client must not miss what changed while it was away, and the node
must not have to buffer per-session history to tell it. The disk structures the
history model already lays down ([RFC 0009](0009-anchors-succession-and-history.md),
[RFC 0011](0011-bptree-index-and-collections.md)) *are* the answer to "what
changed since?" — so catch-up navigates them instead of replaying a log the node
had to keep. Three earlier designs were tried and rejected (below); this one has
no per-session node state to prune, overflow, or lose on restart.

## Design

- **Topic.** `{ struct_hash, pivot: Option<LocalId> }` — `None` = the Unique
  anchor, `Some` = one collection. The tenant never rides a topic; it is the
  connection's bound identity.
- **`Command::Changes`** (payload `(pivot, since: Option<u64>)`). The reply is
  `Change::{Cursor(u64) first, Saved(Id, Metadata, Vec<u8>), Removed(Id, u64)}`:
  - **collection** → recency + dead tail scans past the cursor, merged in instant
    order (each record once, at its live state);
  - **Unique** → the chain walked *forward* from the cursor's derived slot via
    `Next` links, each missed version rebuilt to live form so adopting in order
    replays the chain **byte-identically**;
  - **`since: None` is registration** — answer the current tail as the starting
    cursor, ship no events (a fresh watch starts at "now", never a full replay).
- **Stateless HTTP poll** (`net::sync`, reserved `SYNC_STRUCT_HASH = "WDB.SYNC"`,
  routed before the registry). `SyncRequest { topics: [TopicCursor{topic, since}] }`
  → `SyncReply { events, cursors }`; the node holds no poll state (it runs
  `Changes` per topic through the registry, so unlisted types refuse uniformly).
  The **client** owns the cursors — they survive an outage and are forgotten on
  last unsubscribe.
- **Client surface.** `Db::watch_unique::<T>()` / `watch_collection::<T>(pivot)`
  yield typed `WatchEvent`s and **mirror each into the cache before yielding**, so
  a watcher keeps local reads warm ([RFC 0024](0024-client-db-and-cache.md)).
  Watches multiplex through the manager ([RFC 0021](0021-connection-manager.md))
  or poll with `db.watch_via_polling(interval)`. A watch needs an authenticated
  handle (anonymous subscriptions refuse).

## Deliberate semantics

A poll tick delivers each changed record **once at its live state** — same-record
writes within one tick coalesce (WS push still delivers every mutation). Tests
assert convergence, not event-by-event equality.

## Alternatives

- **Journal commit-cursor ("since sequence N")** —
  [RFC 0028](0028-journal-commit-cursor-sync-DEPRECATED.md): rotated journals are
  deleted, `Batch` frames are physical not logical.
- **Bloom-filter reconnect sync** — [RFC 0029](0029-bloom-filter-screen-sync-DEPRECATED.md):
  answering a filter would force the node to test its whole dataset; exact
  subscriptions give live filtering with no false positives.
- **Node-side per-session poll buffer** — [RFC 0032](0032-node-side-poll-buffer-DEPRECATED.md):
  state to prune/overflow, lost on restart; stateless navigation replaced it.
