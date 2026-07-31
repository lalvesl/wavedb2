# RFC 0025 — The wasm / IndexedDB target

- **Status:** Implemented (M5)
- **Crates:** `wavedb-wasm`, `wavedb::cache`
- **Code:** `IdbStore`, `idb.rs`

## Summary

In the browser, the whole store is **IndexedDB** — a flat `Id → Vec<u8>` mapping,
no pages, no journal, no `data.bin`. The `Store` trait
([RFC 0008](0008-store-trait-and-atomic-batch.md)) absorbs the difference, so
`BpTree`/`Collection`/the whole engine run in-browser unchanged (serverless mode)
and as the client cache ([RFC 0024](0024-client-db-and-cache.md)).

## Motivation

The native engine's machinery (4 KiB pages, WAL, superblock) has no analogue in
the browser and would bloat the wasm binary. IndexedDB already *is* a durable,
transactional key→value store — so the browser backend is a thin bridge, and the
one thing the trait needs (an atomic batch) maps directly onto an IndexedDB
transaction.

## Design

- **`IdbStore`** — one `kv` object store, key = the 128-bit `Id` as 16
  big-endian bytes (bytewise key order == numeric order, so the index's
  key-ordered scans work), value = the wire bytes.
- **`apply` = ONE IDB readwrite transaction** — complete = durable, error/abort =
  rolled back whole. The atomic-batch contract for free
  ([RFC 0008](0008-store-trait-and-atomic-batch.md)).
- **`idb.rs`** bridges event-driven IDB requests to futures (a oneshot per op,
  closures dropped after the await — no per-op leak); faults convert to
  `core::Error::Backend` at the module edge.
- **Derived archive slots need the flat keyspace** — this is *why* an archive's
  `SALT = trunc(STRUCT_HASH)`: types must not collide in one flat IndexedDB store
  ([RFC 0009](0009-anchors-succession-and-history.md)).
- **No tokio.** The wasm build runs on `wasm_bindgen_futures`
  ([RFC 0006](0006-platform-seam.md)); `spawn_local` for the manager task.

## The registry-cost question (the M1 risk, retired)

The `match`-on-`STRUCT_HASH` model ([RFC 0002 §1](0002-architectural-hard-rules.md))
raised a size worry for wasm: does every exposed struct bloat the binary? Measured
(`examples/registry-size`): **~23 B raw / 18 B gzip** per exposed struct for the
pure `match` arm once its decode shape exists, **~204 B raw / 44 B gzip** for a
struct bringing a *novel* decode shape (code a heterogeneous schema needs anyway).
Verdict: no sum type, no descriptor table — the registry scales.

## Proven

Real headless Chrome (`tests/idb_store.rs`, `live_node.rs` via
`scripts/browser_demo.sh`): raw batch roundtrip + reopen durability; the typed
serverless flow (Unique + NonUnique collection + BpTree secondary over IndexedDB);
and a live-node demo — `#[server]` calls, typed Unique save, streamed collection
walk over `fetch`, IndexedDB caching reads.
