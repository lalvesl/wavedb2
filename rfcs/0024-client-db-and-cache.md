# RFC 0024 — The client `Db` and write-through cache

- **Status:** Implemented (M4 + M6)
- **Crates:** `wavedb`
- **Code:** `db.rs`, `client_handle.rs`, `client_cache.rs`, `cache/`

## Summary

`wavedb` is the developer's client. `Db::connect(addr, user, tenant)` is
transport-only; the M6 `Db::open(CLIENT_REGISTRY, addr, user, tenant, app)`
family attaches a **local write-through cache** — WaveDB caching WaveDB. Typed
ops spell `T::get(&db)` / `v.save(&db)` / `T::collection(pivot)` through the
`DbHandle` seam, so one body text runs against a client `Db`, a node `ServerDb`,
or a bare `LocalHandle`.

## Motivation

An app wants (a) a typed surface identical to what a `#[server]` body sees, and
(b) local reads that survive a node blip — without the cache ever *diverging*
from the node. Two ideas serve this: a single `DbHandle` trait so the generated
methods are context-agnostic, and a **node-first** cache that is strictly behind
the node so no merge is ever needed.

## Design — the `DbHandle` seam

One trait all three execution contexts implement (`Db`, `ServerDb`,
`LocalHandle`), so generated methods say `T::get(&db)` regardless of what `db`
is. Walk-shaped ops return `impl Stream` in the trait signature even where the
client buffers internally — the surface is stream-shaped from day one, so
streaming ([RFC 0020](0020-net-transport-dumb-tunnel.md)) is an internal change,
not an API break. `ServerDb` mirrors it node-side for `#[server]` bodies
([RFC 0016](0016-server-functions.md)).

## Design — the cache (cfg-switched like the platform seam)

- **Backend** ([RFC 0006](0006-platform-seam.md)): native = a `PageStore` under
  `~/.cache | XDG_CACHE_HOME | %LOCALAPPDATA%` `/<app>` (the app is the leaf,
  XDG-style, auto-created; `open_at` for an explicit dir); wasm =
  `wavedb::cache::IdbStore` ([RFC 0025](0025-wasm-indexeddb-target.md)).
- **Node-first semantics** (`client_cache.rs`): acknowledged ops mirror
  best-effort under **node-minted** ids (`Collection::adopt`; `All` frames carry
  `(Id, Metadata, T)` so walks mirror under authoritative ids with node chain
  data verbatim). Reads fall back to the cache **only on `Error::Transport`** and
  **only when the cache holds the answer** — absence propagates the fault,
  `NodeError` refusals never fall back. **Offline writes refuse** (queueing is
  future work) — the cache is strictly behind the node, so it can never diverge.
- **`db.local()`** exposes the cache's direct `LocalHandle`.
- **One engine per process** ([RFC 0018](0018-storage-engine.md)): a `Db::open`
  client and a node can't share one, so a cache-and-node test runs the node as a
  child process.

## Design — live sync integration

`db.watch_unique::<T>()` / `watch_collection::<T>(pivot)` yield typed
`WatchEvent`s and **mirror each into the cache before yielding**, keeping local
reads warm; watches multiplex through the manager or poll over HTTP
([RFC 0022](0022-live-sync-navigation-catchup.md), [RFC 0021](0021-connection-manager.md)).

## History note

The interim client surface was `db.get::<T>()` (inherent methods won resolution
over the generated `T::get(store, tenant)`). It was retired once those inherent
methods were re-plumbed onto the `DbHandle` generic — the `T::get(&db)` spelling
is the real one.
