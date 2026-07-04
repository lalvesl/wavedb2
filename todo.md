# TO DO

Clean reimplementation of WaveDB. The docs describe the **target** design; no
code exists yet. Build order, roughly bottom-up:

## Foundations (`wavedb-core` + `wavedb-macros`)

- `Id` (128-bit: `KEY u64 · TENANT u48 · FLAG 1 · SALT 15`) with accessors +
  per-shape `SALT` (Unique `0`; NonUnique/BpTree/Pivot = 15 random bits, no
  struct-hash truncation);
- `LocalId` (80-bit: `KEY u64 · FLAG 1 · SALT 15`) — `Id` with `TENANT` stripped
  for BpTree-internal pointers; `from_id`/`to_id(tenant)` conversions; 10-byte wire;
- `STRUCT_HASH` = **SeaHash** (pinned `seahash` crate) over
  `name + shape + field names + field types`, fixed four-lane WaveDB seed
  (portable across builds/arches/endianness);
- `Metadata` (modification chain, pivot back-link, user, device, permission) —
  **no version field**. Uses `Option<LocalId>` for `old_modification_id`,
  `new_modification_id`, and `pivot_id` (`None` when absent — no sentinel ZERO
  needed). Stack = 18 bytes (3×1 flag + 6 user + 8 device + 1 permission).
  `pivot_id` = owning Pivot back-link (NonUnique reindex); stamped at `insert`,
  `None` for Unique; outside `STRUCT_HASH`;
- `WaveWire` trait + `#[derive(WaveWire)]` (trait + derive share the name, like
  `Clone`; no serde, no `repr(C)`); see `docs/wire_format.md`;
- index contracts in **core** (portable, `Store`-generic): `Store` (`get` +
  atomic `apply(batch)`), `IndexKey` (order-preserving key encoding),
  `Pivot` (`current`/`dead`/`secondaries` roots as `LocalId` — tenant stripped —
  **+ collection-default `permission`**), `BpTree<S: Store>` (`at(LocalId)`,
  `search` → `Id` stream, `insert`/`remove` take record `Id` return `LocalId`
  root; byte-compare on encoded keys), `IdStreamExt` (`intersect`/`union`/`except`).
  Pages/journal live behind `Store`, not here;
- `#[wavedb]` macro: shapes `Unique` (default) / `NonUnique`; generate the
  `Pivot` + `BpTree` _types_; `PivotId` field references for nesting;
- explicit `create_pivot` (one per tenant per definition) → `PivotId` stored in a
  `Unique` or nesting `NonUnique`; never auto-created;
- schema-evolution hooks: `first_try` (pre-search) + `fallback_not_found`
  (post-miss). No migration chains;
- permissions: tenant-only / public / tenant-list (group deferred). NonUnique is
  **two-level** — `Pivot` holds the collection default (seeds inserts, gates
  collection-scope ops), each record's `Metadata` overrides (authoritative; keeps
  `Update` atomic, no `Pivot` read).

## Exposure (derive ops + explicit declaration) — replaces `wavedb-build`

- **remove `wavedb-build`** (the `build.rs` `src/`-scanner + generated registry)
  — no build step, no auto-discovery, no `include!(OUT_DIR…)`;
- division of labor: `#[wavedb]` / `#[server]` do **all** per-item codegen —
  `STRUCT_HASH`, `WaveWire`, `PivotId`/`Pivot`/`BpTree`, hooks, **and the
  execution steps** (Unique `get`/`save`; NonUnique
  `insert`/`update`/`remove`/`get`/`search`; the server-fn call arm) as
  generated fns on the item — defined, not yet reachable;
- `expose_server!` / `expose_client!`: explicit declaration module per side
  listing what that side serves / can call; expands to the per-`STRUCT_HASH`
  `match` per operation (wire parse, hooks, `Pivot`/`BpTree` access, engine
  ops, server-fn dispatch) — static, monomorphized, no `Object` enum.
  **The lists are the registry**;
- **hard constraint: NO `dyn` in the expansion** — no trait objects, no
  fn-pointer tables, no runtime registration; overrides substitute the path
  **inside the match arm** at expansion time (compiler-resolved, inlinable),
  never a stored callback;
- unlisted item = unknown hash at that boundary — storage-only types possible
  (`Credentials`/`Session` pattern: read inside server-fn bodies, never
  wire-addressable); entries support per-op **exclusion** (`remove: never`)
  and **override** (`save: audited_save`) to harden or reshape the surface;
- entries are plain Rust paths, so the old scanner limits go away:
  dependency-crate, `cfg`-gated, and macro-generated items all declarable;
- migrate `examples/schema-smoke` off `build.rs` + `include!` onto exposure.
- _Future:_ `update_call` kind; secondary indexes via `#[wavedb::pivot(field)]` /
  `#[wavedb::pivot((f1, f2))]` (extra `BpTree` + `Pivot` root + `by_field` lookup).

## Storage engine (`wavedb-storage`)

- block manager: alloc/free/coalesce/truncate runs of 4 KiB blocks, journaled;
- per-`STRUCT_HASH` `Vec<u64>` page directory; one block descriptor
  (`u40 start · u20 count · u4 occupation`) shared by pages **and** dictionary;
- `hash_of(id)` = SeaHash over the `u128`'s 16 LE bytes, seeded by a **per-DB random `[u64;4]`
  in data.bin page 0**; result feeds linear hashing;
- **linear hashing** (`index` / `split_next`), 16 KiB first page, grow-in-place +
  background `split_next`;
- `PageFormat` derive trait per page kind (Unique / NonUnique / Pivot / BpTree):
  `crc32 + STRUCT_HASH + id-list + blob`, `WaveWire` ser/deser;
- BpTree pages = **32 KiB** (8 × 4 KiB blocks); **one node per page**. Entry:
  `key (8 B) + LocalId (10 B) = 18 bytes` — same format for internal (child page)
  and leaf (record pointer). Capacity ≈ **1 819 entries/page**. Tree height:
  ≤3.31 M records → 2 page reads; ≤6.03 B → 3 reads. Split: immediate on fill,
  single journal entry, `Pivot` updated only on root split. Merge on delete:
  node < 25% fill → merge or redistribute with sibling (single journal entry).
  Leaf `LocalId` inflated to `Id` via `to_id(tenant)` — never disk;
- per-`STRUCT_HASH` dictionaries + dictionary directory (same block descriptor);
- write pipeline: journal-first → in-memory `BTreeMap<Id>` cache → background
  settle → background rebalance; journal replay on startup;
- **atomicity = the cache**: a multi-record op (record + `BpTree`) is one journal
  entry applied to the cache atomically; no separate txn manager. `Pivot` has no
  counter, so it is read-not-written on a normal NonUnique op;
- NonUnique record's identity `Id` is fixed at `insert` (stable anchor). `save`
  (update) **force-reindexes every live tree** — `current` + each secondary —
  reaching roots via `Metadata.pivot`; `dead` is **not** touched (history = the
  `Metadata` chain), so only `remove` writes `dead`. IO: Unique `save` = 4;
  NonUnique `save`/`insert`/`remove` = `7 + 2N` (N = secondary indexes).

## Storage traits (core seam)

- core `Store` trait = the **client-side local store** (`get`/`update`/`remove`
  over `Id` + wire bytes; async, no I/O — contract only). native impl = file kv;
  wasm impl = IndexedDB. **Not** the node engine;
- typed per-struct traits (macro, by shape): `UniqueObject` (`get`/`save`),
  `NonUniqueObject` (`collection` → `insert`/`get`/`all`/`remove`, record `save`).
  Each call = **local `Store` write-through + network send** via the `Db` handle;
- the authoritative `Pivot`/`BpTree`/page engine runs on the **node**
  (`wavedb-storage`), reached over `wavedb-net`. `Id` is client-known (Unique
  deterministic, NonUnique minted at insert) so write-through is immediate.

## Client (`wavedb`)

- `Db::connect` / `Db::open` family (native file + wasm IndexedDB);
- typed CRUD: Unique `get`/`save`; NonUnique `insert`/`save`/`remove` + collection
  walk via `Pivot`/`BpTree`; explicit `create_pivot`. No query DSL.
- each write = **local `Store` write-through + a command frame** over the
  transport (HTTP POST for now); `save()` emits the `Update` command for NonUnique;
- collection reads are **async iterators**: `all` / `by_<field>` (and
  collection-returning `#[server]` fns) return `impl Stream<Item = Result<T>>`, not
  a buffered `Vec`; `.try_collect().await?` to materialise. Prelude re-exports
  `Stream`/`StreamExt`.

## Server functions (`#[server]`) — replaces query

- `#[server]` proc-macro: server-only async body + client call binding;
- a function's `STRUCT_HASH` (SeaHash over fn name + each arg/return object's
  `STRUCT_HASH`, no separate `FN_HASH`) is its identity; args/return via `WaveWire`. A
  collection return is a `Stream<Item = Result<T>>` whose items ship one at a time
  (back-pressured), re-exposed as an async iterator client-side — not a buffered `Vec`;
- transport: the **same `CommandFrame`** `{ struct_hash, command, payload=args }`
  as an object op — no separate call frame; the single `match struct_hash`
  disambiguates (function arm ignores `command`, decodes `payload` as args);
- body never enters the client binary; permission checks run in the body;
- **auth: login-required by default**; `#[server(public)]` opens a fn to the
  unauthenticated tier (`login`/`refresh`). The macro injects the auth guard into
  the **body**, not the registry `match` (uniform `struct_hash → body` dispatch —
  simpler builder); identity is the verified Access token, never the request body.

## Nodes & transport (`wavedb-quick-node`, `wavedb-net`)

- **single node first** — durability = journal; ring/replication/failover deferred;
- request envelope `{ auth: access_token, frame: CommandFrame }`; **one uniform
  frame** `{ struct_hash, command, payload }` for both object ops AND server-fn
  calls (functions + structs share the hash space — can't tell apart at the frame,
  only `match struct_hash` can); `command` = `Get`/`Save` (Unique) |
  `Insert`/`Update`/`Remove` (NonUnique), ignored for a function (hash = the op);
  dispatch = `match struct_hash` → struct: `match command` to engine fn / function:
  decode `payload` args + run body;
- **transport = dumb tunnel**: no HTTP headers/cookies/status — auth (access
  token), command, and errors all ride **inside** the WaveDB envelope (the POST
  body is self-contained). **HTTP POST only for now** (token re-sent each request);
  WebSocket sends the token once at handshake (deferred), with push / Bloom sync;
- node-side enforcement gates: identity (verified access token from the envelope,
  not HTTP headers / unsigned fields) → header → decode → permission → `validate`
  → `preprocess`;
- server-function dispatch by `STRUCT_HASH` (same per-hash `match` as structs);
  auth/permission inside the body.

## Browser (`wavedb-wasm`)

- IndexedDB key→value adapter (no pages, no journal); same typed `Db`.

## Deferred

- **Cold tier (slow-node) removed** — history single-tier in data.bin, unbounded
  growth accepted; pruning/compaction/archive later;
- **Permission groups**;
- `STRUCT_HASH`-grained write-ownership (tenant-only for now);
- cross-tenant read _path_ (multi-node routing + grant enforcement) — model
  stays, serving path deferred;
- offline-first reconciliation.

## Resolved bit budgets

- **ID** = `KEY u64 + TENANT u48 + FLAG 1 + SALT 15 = 128`. No reserved bits.
- **LocalId** = `KEY u64 + FLAG 1 + SALT 15 = 80` (10 bytes). `Id` without `TENANT`
  for BpTree-internal pointers — tenant known from tree scope.
- **Block descriptor** = `start u40 + count u20 + occupation u4 = 64`
  (~4 PiB/file, ~4 GiB/page, 1/16th occupation). One format for pages **and**
  dictionary.

# DOING

- **Storage engine** (`wavedb-storage`): durable single-node `Store` landed
  (`BlockFile` + `SlotPage` page format + `directory` split + `Journal` +
  `PageStore` cache/replay pipeline) and the **`Store`-generic `BpTree` now
  lives in core** (`wavedb_core::index`), including **merge/rebalance on
  delete** and search-descent pruning. The **typed collection layer is in**
  (core `Collection<T>` + macro-emitted `collection()`/`create_pivot()` and
  Unique `get`/`save`) and the M2 NonUnique path is proven end-to-end through
  it (`tests/nonunique_collection.rs`: derived API → PageStore → durable
  reopen).
  **Remaining for M2:** the dedicated **32 KiB one-node-per-page** BpTree format
  (nodes currently ride the generic `SlotPage` directory under a reserved
  page-kind `STRUCT_HASH`); **background** settle / rebalance (settle is inline
  with `apply` for now); per-value (strings/blobs) heap compression.
  (Secondary indexes and per-record `Metadata` + the version chain: **done** —
  see DONE.)
- **Design note (M3):** `PageStore` is **cache + journal authoritative** for
  reads; `data.bin` is a deterministic projection rebuilt by journal replay on
  open. It settles a value into the per-`STRUCT_HASH` page directory by reading
  the `STRUCT_HASH` from the value's first 8 bytes — so every stored value
  (records **and** BpTree nodes) must be `STRUCT_HASH`-headed. Typed
  per-command settling (knowing record vs index-node vs Pivot page kind) is the
  M3 node layer's job.

# DOING (next after storage)

- **Exposure — struct surface DONE** (see DONE); remaining: `#[server]`
  functions in the same hash space (M4 — needs `Db`), streaming reads
  (`All`/search) over a transport, and `examples/todo-app`'s functions-only
  allowlist (blocked on `#[server]`).
- **M3 node**: exposure-linked `wavedb-quick-node` driving `PageStore` by typed
  command dispatch (`Exposure::execute` is the seam it consumes); HTTP POST
  transport; node-side gates.

_164 tests, clippy-clean (pedantic + nursery). Workspace members: wire,
wire-derive, core, macros, storage, schema-smoke._
