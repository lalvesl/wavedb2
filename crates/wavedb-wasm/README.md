# wavedb-wasm

The **browser client**. All code is gated on `target_arch = "wasm32"`; on native
targets the crate is empty (it exists so `cargo test --workspace` compiles). The
public `Db` API is identical to native — only the storage adapter and runtime
change shape.

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
targets. Filtered/derived reads are server functions (run on the node), so the
wasm client just ships the call. Sync needs no page parity either: the Bloom-filter
protocol exchanges objects, never pages. (`localStorage` — synchronous,
string-only, ~5 MB — cannot fill this role.)
