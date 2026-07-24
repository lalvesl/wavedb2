# wavedb-wasm

The **browser client**. All code is gated on `target_arch = "wasm32"`; on native
targets the crate is empty (it exists so `cargo test --workspace` compiles). The
public `Db` API is identical to native — only the storage adapter and runtime
change shape.

> **Status:** M5 in progress. The platform seam (`wavedb-platform`) is
> live: the whole client stack (`wavedb-core`, `wavedb-net`, `wavedb`)
> compiles for wasm32-unknown-unknown — timestamps from `Date.now()`,
> entropy from `crypto.getRandomValues`, the tunnel over browser `fetch` —
> and this crate is a workspace member shipping one raw `probe` export
> (anchors the stack for the size tracker). The IndexedDB `Store`, the
> WebSocket runtime notes below, and the typed browser demo (the M5 exit)
> are **not built yet**.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Runtime swap

The WASM build replaces the Tokio runtime: futures run via
`wasm_bindgen_futures`, HTTP goes through the browser `fetch` API, WebSockets
through `gloo_net::websocket`, and persistence through IndexedDB.

## Storage: key→value, not pages, no journal

The native engine manages 4 KiB pages and a write-ahead journal because it owns
the physical disk layout. In the browser it owns none of that — IndexedDB is
already a durable, ordered key→value store doing its own block management and
crash safety. Emulating WaveDB pages on top would stack two block managers and
pay write amplification for zero IO benefit. So the WASM build **skips the page
layer and the journal entirely**:

- **Key = the 128-bit `Id`** (big-endian). The Unique anchor lives at its
  directly computable key; NonUnique records, Pivots, and BpTree nodes at their
  timestamp-keyed `Id`.
- **Value = the wire-encoded record**, compressed exactly as on native.
- Because `Id` orders by its most-significant `KEY` field, IndexedDB's ordered
  keyspace groups records **by type and time** (Unique by `STRUCT_HASH`,
  NonUnique by `CREATED_AT`). Tenant/collection scoping is provided by the same
  `Pivot` → `BpTree` index layer the engine uses on native — not by the raw key
  order — so a collection read walks the tenant's own B+tree, never the whole
  store.
- The page-pressure machinery (page growth, bucket splits, the block allocator)
  simply does not run client-side. Quota is the browser's job; the local store is
  a write-through cache that can always be re-fetched from the cluster.

The engine layers above storage — the Unique anchor, history chains, the
schema-evolution hooks, and the `Pivot`/`BpTree` index — are identical on both
targets, because they are the `Store`-generic
[index contracts](../wavedb-core/README.md#index-contracts--pivot-bptree-indexkey)
in `wavedb-core`. The web build supplies an **IndexedDB `Store`** and the _same_
`BpTree`/`Pivot` code runs on it — pages and journal are `PageStore` internals, not
the index. A thin client still ships filtered reads to a node; a **serverless** app
(static files from a CDN, no node) links the engine and runs the `BpTree` walk +
`#[server]` bodies **in-browser over IndexedDB**, authoritative locally. Sync needs no page parity either: the Bloom-filter
protocol exchanges objects, never pages. (`localStorage` — synchronous,
string-only, ~5 MB — cannot fill this role.)
