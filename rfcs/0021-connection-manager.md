# RFC 0021 — The connection manager

- **Status:** Implemented (landed 2026-07-16, user-directed — M7 W5.5)
- **Crates:** `wavedb-net`
- **Code:** `crates/wavedb-net/src/manager/` (`actor.rs`, `ws_conn.rs`, `poll.rs`,
  `boot.rs`)

## Summary

**One never-ending background task per process** owns *every* exchange with a
node. It is the single place connections are dialed, shared, and torn down — all
POSTs run through it, and all watches of one `(addr, identity)` multiplex over
**one** WebSocket connection. It boots lazily on first use and never ends.

## Motivation

Watches, POSTs, and reconnection all need a place to *live* that outlives any
single request and is shared across the process. Without it, every watch would
open its own socket (N screens = N connections), reconnection would have nowhere
to happen, and the offline queue / reconnect cursor would have no owner. Making
one actor the sole authority gives multiplexing, a clean lifecycle, and the
natural seam for [RFC 0022](0022-live-sync-navigation-catchup.md)'s reconnect
cursor and the future offline queue.

## Design

- **The task.** Native = a dedicated thread with a current-thread runtime +
  `LocalSet` (`wavedb_platform::task::spawn_detached`); wasm = a detached
  `spawn_local` — no tokio in wasm ([RFC 0006](0006-platform-seam.md)).
- **All POSTs route through it** (`NetClient` internals re-plumbed; public API
  unchanged). The establish-vs-mid-stream error split is preserved so the M6
  cache can fall back on an establishment fault ([RFC 0024](0024-client-db-and-cache.md)).
- **Watches multiplex.** Every watch of one `(addr, identity)` shares ONE
  WebSocket connection — `Hello` once, one wire subscribe per topic, events fanned
  out per topic. No pumping falls on watchers: the connection's reader task pushes
  each event into the right channel (`ws::Conn::split()` is the platform seam).
- **Lifecycle authority is the manager loop**, not the actors (`actor.rs`): it
  counts each actor's watchers by id and drops the actor's channel when the last
  unregisters — so a later watch always gets a *fresh* dial and no ack can race a
  dying actor. A dead actor (socket broke) is detected by its closed channel.
- **Watches can ride plain HTTP** (`WatchMode::HttpPoll`): a per-identity poll
  loop asks "anything new?" on a timer for clients that cannot hold a WebSocket
  open — the sync mechanism is [RFC 0022](0022-live-sync-navigation-catchup.md).

## Known seam

The manager is the intended home for **WS reconnect catch-up** — today a
transient socket loss ends the watch streams; making them survive is
[RFC 0034](0034-ws-reconnect-catchup.md) — and for the future **offline write
queue** ([RFC 0036](0036-offline-write-queue-WIP.md)).

## Alternatives

- **One connection per watch** (the W5 shape before this): simple but N-screens =
  N-sockets and no shared reconnect authority — replaced by multiplexing.
