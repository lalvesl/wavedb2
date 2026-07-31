# RFC 0023 — The quick-node and enforcement gates

- **Status:** Partial (node + gates 1–3 built; gates 4–6 are seams)
- **Crates:** `wavedb-quick-node`
- **Code:** `server.rs` (`Server`/`Bound`), `dispatch.rs`, `subscribe.rs`,
  `serve_ws.rs`

## Summary

`wavedb-quick-node` is a **library** (no bin) that turns the storage engine into
*your* backend: `Server::new(REGISTRY).data_dir(d).serve(addr)`. Attaching the
`expose_server!` output ([RFC 0017](0017-exposure-registry-and-side-features.md))
opens the `PageStore` and drives it by typed command dispatch through
`Exposure::execute`. Every request passes an ordered chain of **enforcement
gates** before the engine.

## Motivation

A generic node is not a backend; the *schema* makes it one. Because the registry
is a `match` the schema crate already emits (dispatch **and** `StorageRegistry`),
"be my backend" is one call: `.registry(REGISTRY)`. And because every op crosses
the same dispatch, security checks belong in one ordered gauntlet, not scattered
per handler.

## Design — the builder

- `Server::new(REGISTRY).data_dir(d).serve(addr)` — the aspirational
  `QuickNode::builder()` spelling is dead; this is the real one.
- `.registry(REGISTRY)` alone opens the engine (the registry emits
  `StorageRegistry`). One open `PageStore` per process
  ([RFC 0018](0018-storage-engine.md)).
- `Server::secret([u8;32])` sets the node's HMAC secret, else a random one per
  boot, published process-wide for the minting helpers
  ([RFC 0026](0026-auth-tokens.md)).

## Design — the enforcement gates (in order)

1. **Identity** — verify the access token; bind the caller `{ user, tenant }`.
   The verified identity threads the whole stack (`Exposure::execute` → generated
   `__wavedb_*` steps → `ServerDb::for_caller`). **Built.**
2. **Header** — `Exposure::knows` (is this hash served at all?). **Built.**
3. **Decode** — `decode_check` the payload. **Built.**
4. **Permission** — record `Metadata.permission` / Pivot default. **Seam**
   ([RFC 0013](0013-permissions.md)) — deferred with the cross-tenant read path.
5. **`validate`** — application hook. **Seam (M8+).**
6. **`preprocess`** — application hook. **Seam (M8+).**

## Design — live push

`SubTable` + `NotifyStore<S>` (a concrete `Store` wrapper, no `dyn`) override
`note_mutation` ([RFC 0008](0008-store-trait-and-atomic-batch.md)) to route
events into per-connection senders keyed `(tenant, Topic)` — exact match,
O(subscribers-of-this-topic), dead senders pruned on publish. The `serve_ws`
session loop binds identity once at `Hello`, runs `Call`s FIFO, and mutates
subscriptions under the caller's tenant ([RFC 0022](0022-live-sync-navigation-catchup.md),
[RFC 0023]—this doc). `sync_poll` executes `Changes` per topic through the same
registry, so the stateless poll path refuses unlisted types uniformly.

## Deferred

- Gates 4–6 (permission/validate/preprocess hooks).
- Multi-node ([RFC 0037](0037-multi-node-cluster-PLANNED-LOW.md)): ring ownership,
  gossip, replication, routing/failover — the target design lives in the crate
  README; **single node only** today
  ([RFC 0007](0007-tenancy-and-data-ownership.md)).
