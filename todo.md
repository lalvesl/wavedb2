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
  with `apply` for now); secondary indexes (`#[wavedb::pivot(...)]`) driven
  through `Collection` (`by_<field>` walks, reindex on `save`); per-record
  `Metadata` written alongside records; per-value (strings/blobs) heap
  compression.
- **Design note (M3):** `PageStore` is **cache + journal authoritative** for
  reads; `data.bin` is a deterministic projection rebuilt by journal replay on
  open. It settles a value into the per-`STRUCT_HASH` page directory by reading
  the `STRUCT_HASH` from the value's first 8 bytes — so every stored value
  (records **and** BpTree nodes) must be `STRUCT_HASH`-headed. Typed
  per-command settling (knowing record vs index-node vs Pivot page kind) is the
  M3 node layer's job.

# DOING (next after storage)

- **Exposure**: implement `expose_server!` / `expose_client!`
  (per-`STRUCT_HASH` `match` over the listed items, per-op exclusion/override,
  emitted `REGISTRY`). The execution steps now exist as derive-generated fns
  (`get`/`save`, `collection()`'s op set) — exposure makes them
  wire-reachable. `examples/todo-app` already shows the target: no `build.rs`,
  functions-only allowlist, structs storage-only.
- **M3 node**: exposure-linked `wavedb-quick-node` driving `PageStore` by typed
  command dispatch; HTTP POST transport; node-side gates.

# DONE

- **`wavedb-wire`** — the `WaveWire` codec extracted into a standalone crate (only
  `thiserror`): trait + `Cursor` + builtin impls + `to_wire`/`from_wire` + its own
  `Error`. No `STRUCT_HASH`, registry, `Id`, or engine coupling — pure value ⇄
  bytes, decode fails only on a buffer/size mismatch (`UnexpectedEof`) plus
  intrinsic per-type checks. The trait is named `WaveWire` (renamed from `WaveWire`);
  trait + derive share the name like `Clone`. `wavedb-core` re-exports it as
  `wavedb_core::wire` **and directly** at the crate root (`wavedb_core::WaveWire`),
  and wraps its `Error` via `#[from]`, so every existing path keeps working.
- **`wavedb-wire-derive`** — the wire crate's own `#[derive(WaveWire)]` proc-macro
  (serde/serde_derive pattern; re-exported as `wavedb_wire::WaveWire`), emitting
  `::wavedb_wire::` paths. Supports structs (named/tuple/unit) **and enums** (the
  canonical tag form — `tag u8 [+ payload-len u32]`, declaration-order tags). Used
  to **replace the hand `WaveWire` impls** in core for `Id`, `LocalId`, `Metadata`,
  and `PermissionRef` (byte-identical — existing roundtrip/length tests pass
  unchanged). `U48` stays hand-written (6-byte 48-bit packing, not field-derivable).
  The older `wavedb-macros::WaveWire` (emits `wavedb_core::wire` paths, struct-only)
  is left for `#[wavedb]`; the two derives coexist by path target.
- **`wavedb-core`** — `Id`, `LocalId`, `U48`, `Metadata`, `PermissionRef`,
  `WaveWire` (re-exported from `wavedb-wire`, also at the crate root; the first four
  derive it, `U48` hand-written), `Error`. Plus the portable contracts: `WaveDbStruct` +
  `Shape`, `Store` (+ `Write`), `LookupHooks`,
  and the `index` layer — `IndexKey` (order-preserving), `Bound`, `Pivot`, `BpTree`,
  `IdStreamExt` (intersect/union/except stream adapters).
- **`wavedb-macros`** — `#[derive(WaveWire)]` (named/tuple/unit) and `#[wavedb]`
  (Unique/NonUnique): emits `STRUCT_HASH`, `WaveWire`, inherent consts
  (`SHAPE`/`HAS_VALIDATE`/`HAS_PREPROCESS`), `WaveDbStruct`, and
  for NonUnique the generated `{Name}PivotId` + `{Name}Pivot`. `#[wavedb::pivot(...)]`
  parsed/stripped → secondary-index count. `#[server]` deferred to M4 (needs `Db`).
  - **`STRUCT_HASH` uses SeaHash (pinned crate)** — portable across arch/endianness so
    client and server agree on identity; the crate is version-pinned so identity can't drift.
- **`wavedb-build` removed** — the `src/`-scanner + generated registry are gone
  from the workspace: derive-generated execution steps + explicit
  `expose_server!`/`expose_client!` declaration replace them — see the Exposure
  section above.
- **`examples/schema-smoke`** — end-to-end M1 proof: `#[wavedb]` derive output
  (`STRUCT_HASH`, roundtrip, shape consts, generated Pivot types) exercised
  directly — no `build.rs`, no `include!`. (Real example; `todo-app` still
  needs M4 `#[server]`/`Db`.)
- **`wavedb-storage` foundations** — `block` (`BlockDescriptor` u40·u20·u4 packing,
  `Run`, `BlockAllocator`: best-fit alloc / coalescing free / tail truncate) and
  `directory` (linear-hashing `bucket_index`/`next_split_bucket`, `Directory`).
  - **Page `hash_of` is SeaHash** — portable across arch/endianness, so journal replay
    rebuilds `data.bin` with the same routing even when the file moves machines. Random
    per-DB seed keeps DoS resistance.
- **`wavedb-storage` engine (M2 durable single-node `Store`)** —
  - **`BlockFile`** — `data.bin` as block-addressed file: superblock in block 0
    (magic + format version + per-DB seed, reserved via `RESERVED_BLOCKS`),
    positioned `pread`/`pwrite` run I/O, grow/truncate, `fsync`.
  - **`SlotPage`** — homogeneous record page: `crc32 + struct_hash + total_len +
    id-list + blob`, crc-verified, reads correctly from a zero-padded run.
  - **`directory` page I/O** — `read_page`/`upsert_record`/`remove_record` and
    `split_next` (the page-moving half of linear hashing: repartition by the next
    hash bit, crash-safe descriptor reorder).
  - **`Journal`** — append-only WAL of `Write` batches; `fsync` on append =
    durability point; torn-tail-tolerant replay (truncates a half-written frame).
  - **`PageStore`** — implements core `Store` (`get`/atomic `apply`): journal-first
    → in-memory `BTreeMap` cache → inline settle into per-`STRUCT_HASH` pages, with
    split-on-growth. `open` rebuilds cache + pages + allocator by journal replay.
    `StorageError`→`Error::Backend` bridge added to core.
  - **core `BpTree`** (moved from storage's `PageBpTree`; the `BpTree` *trait*
    was deleted — one concrete `Store`-generic type in `wavedb_core::index`
    carrying `tenant`, shared by `PageStore` and the future IndexedDB store).
    Keys by the record's unique 10-byte `LocalId` (order = `CREATED_AT`).
    Insert with full leaf/internal split + cascade + root growth; idempotent;
    `search` streams record `Id`s by a `CREATED_AT` `Bound` **with descent
    pruning**; `remove` with **merge / redistribute / root-collapse**
    (underfull = <¼ cap; merge when the pair fits ¾ cap), all invariants
    checked by a test harness. Nodes encode via `WaveWire` behind a reserved
    page-kind tag and settle as ordinary `PageStore` values.
  - **Checked wire framing** — the WaveWire rule is fully
    in effect: `Write` derives `WaveWire` and journal frames are
    `[len][to_wire_checked(Vec<Write>)]`; the superblock body is
    `[magic][to_wire_checked(SuperblockBody)]` (version + seed inside the crc);
    and `SlotPage` is `[len][to_wire_checked(PageBody)]` (`struct_hash` +
    `(id, bytes)` entries — the hand-rolled header/id-list/offset format is
    gone). No engine structure hand-rolls its byte layout anymore; the only
    raw prefixes are the superblock magic and the `u32` payload length that
    delimits a page in a zero-padded run / a frame in the log.
  - **Hygiene** — 350-line-per-file budget enforced by
    `scripts/check_file_length.sh` (CI step); `maybe_split` checks only the
    touched bucket (O(1)); `wavedb-build` removed from the workspace.
  - **Per-`STRUCT_HASH` dictionaries + zstd page compression** — raw-content
    (no trainer) capped append-only sample buffer per type (`dictionary`
    module); **version = prefix length** (append-only ⇒ every old state is a
    prefix of the live buffer — old pages stay readable with no recompression
    or superseded-run bookkeeping); persisted in `data.bin` as its own block
    run via the shared allocator, rebuilt + re-persisted by journal replay.
    Page bodies store as a `PagePayload` enum: `Zstd { dict_len, raw_len,
    bytes }` or `Raw` — per-type opt-out (`Directory::with_compression`;
    `PageStore` disables zstd for hot `BpTree` node pages) plus automatic
    `Raw` fallback when zstd cannot shrink a body. `directory` split into
    container/math + `directory_pages` (page I/O) for the file budget.
- **Per-type compile-time storage (`StructStorage`)** — the engine's runtime
  `HashMap<STRUCT_HASH, Directory>` + store-wide mutex are gone:
  - `#[wavedb]` emits (native only, `#[cfg(not(target_arch = "wasm32"))]`) one
    `static wavedb_storage::StructStorage` per declared type **and** per
    generated `{Name}Pivot` — the type's own cache (`RwLock<BTreeMap>`) and
    `Directory` slot (`Mutex<Option<…>>`), reached as `T::struct_storage()` /
    `T::storage_mem_cache()` / `T::storage_directory()`; schema crates gain a
    target-gated `wavedb-storage` dep (wasm expansion omits the slots).
  - `PageStore::open(dir, &[&'static StructStorage])` takes the slots as an
    **explicit registry** (`T::storage_entries()` = record + Pivot slots; the
    reserved BpTree-node slot auto-registers, compression off) — sorted-slice
    binary search, allowlist semantics: an unlisted hash is refused
    (`UnregisteredStructHash`) *before* journaling. One open store per process
    (`EngineBusy` otherwise) since the slots are process-global statics.
  - Locking split: journal `Mutex` (append + cache commit under it ⇒ cache
    order == journal order), allocator `Mutex`, and per-type locks for
    everything else — reads (`Store::get_of`, new trait method with a `get`
    fallback default; `Collection`/`BpTree` pass their compile-time hashes)
    touch only their own type's cache lock. Settle converges pages to the
    cache's current bytes (idempotent, order-independent projection).
- **Typed collection layer** — the developer-facing surface over the (internal)
  `BpTree`, in the exact target shape
  (`Todo::collection(pivot, tenant).insert(&store, &todo)`):
  - core **`Collection<T: NonUniqueStruct>`** — `create` / `insert` / `save` /
    `remove` / `get` / `search` / `all`. Each mutating op is **one atomic
    `Store::apply` batch** (record + touched B+tree nodes via the new `plan_*`
    planners + `Pivot` rewrite when a root moved). Records/pivots are enveloped
    `[STRUCT_HASH (8 B LE)][wire]`, decode-verified (`UnknownStructHash` on a
    foreign id). Record `Id`s minted `KEY = CREATED_AT` nanos, `FLAG = 0`,
    counter salt; `remove` moves `current` → `dead` keeping the bytes (history
    navigable). New core errors: `PivotMissing`, `RecordMissing`.
  - **trait seams** — `NonUniqueStruct { type Pivot }` (macro-implemented, so a
    `Unique` type can't be collection-driven at compile time); `Pivot` gained
    `const STRUCT_HASH` (own identity, hashed under a reserved `Pivot` shape
    discriminator) and `replace_roots()`.
  - **macro emission** — `#[wavedb(NonUnique)]` emits `collection(pivot_id,
    tenant)` + `create_pivot(store, tenant)`; `#[wavedb]` (Unique) emits
    anchor `get(store, tenant)` / `save(store, tenant)` (save = upsert, no
    create). Proven end-to-end in `schema-smoke`
    (`derived_collection_flow_end_to_end`) and over the durable engine in
    storage's `nonunique_collection.rs` (insert/save/remove survive reopen).
- **`examples/todo-app` on the exposure architecture** — the last
  `build.rs`/`include!(registry)` remnant replaced with `expose_server!` /
  `expose_client!` declaration modules (functions-only allowlist; all structs
  storage-only — `Auth`, the username registry, `Profile`, `Todo` are never
  wire-addressable; `REGISTRY` now comes from `expose_server!`). Aspirational
  (workspace-excluded) but architecture-correct.

_146 tests, clippy-clean (pedantic + nursery). Workspace members: wire,
wire-derive, core, macros, storage, schema-smoke._
