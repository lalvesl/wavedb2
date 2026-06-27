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
  `Pivot` (`current`/`dead`/`secondaries` roots as `LocalId` — tenant stripped),
  `BpTree<S: Store>` (`at(LocalId)`, `search` → `Id` stream, `insert`/`remove`
  take record `Id` return `LocalId` root; byte-compare on encoded keys),
  `IdStreamExt` (`intersect`/`union`/`except`). Pages/journal live behind `Store`,
  not here;
- `#[wavedb]` macro: shapes `Unique` (default) / `NonUnique`; generate the
  `Pivot` + `BpTree` _types_; `PivotId` field references for nesting;
- explicit `create_pivot` (one per tenant per definition) → `PivotId` stored in a
  `Unique` or nesting `NonUnique`; never auto-created;
- schema-evolution hooks: `first_try` (pre-search) + `fallback_not_found`
  (post-miss). No migration chains;
- permissions: tenant-only / public / tenant-list (group deferred).

## Registry generation (`wavedb-build` + `build.rs`)

- division of labor: `#[wavedb]` does per-struct codegen (`STRUCT_HASH`, `Wire`,
  `PivotId`/`Pivot`/`BpTree`, hooks); `wavedb-build` only **aggregates**;
- `generate_registry()` scans this crate's `src/` **only** (no deps, no `cfg`
  expansion, no macro-generated structs) and references macro-emitted paths
  (`module::Struct::STRUCT_HASH`); deps must re-export into `src/` to register;
- emit `$OUT_DIR/wavedb_registry.rs`: `Object` enum (`STRUCT_HASH` → variant),
  `Object::from_wire`/`to_wire`, hook routing (`first_try`/`fallback_not_found`),
  `Pivot`/`BpTree` accessors, server-fn dispatch — static `match`, no `dyn`;
- schema crate pulls it in with `include!(concat!(env!("OUT_DIR"), …))`.
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
  `crc32 + STRUCT_HASH + id-list + blob`, `Wire` ser/deser;
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
- collection reads are **async iterators**: `all` / `by_<field>` (and
  collection-returning `#[server]` fns) return `impl Stream<Item = Result<T>>`, not
  a buffered `Vec`; `.try_collect().await?` to materialise. Prelude re-exports
  `Stream`/`StreamExt`.

## Server functions (`#[server]`) — replaces query

- `#[server]` proc-macro: server-only async body + client call binding;
- `FN_HASH` (name + arg types + return type) identity; args/return via `Wire`. A
  collection return is a `Stream<Item = Result<T>>` whose items ship one at a time
  (back-pressured), re-exposed as an async iterator client-side — not a buffered `Vec`;
- transport `CallServerFn { fn_hash, args }` over `wavedb-net`; registry dispatch;
- body never enters the client binary; permission checks run in the body.

## Nodes & transport (`wavedb-quick-node`, `wavedb-net`)

- **single node first** — durability = journal; ring/replication/failover deferred;
- node-side enforcement gates (header → decode → validate → preprocess);
- server-function dispatch by `FN_HASH`;
- WS / HTTP transports; Bloom screen-sync.

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

- **Storage engine** (`wavedb-storage`): `block` + `directory` landed; next is the
  block-backed file (`BlockFile`), page format (crc32 + id-list + blob), BpTree page
  split/merge, dictionaries, and the journal/cache pipeline.

# DONE

- **`wavedb-wire`** — the `WaveWire` codec extracted into a standalone crate (only
  `thiserror`): trait + `Cursor` + builtin impls + `to_wire`/`from_wire` + its own
  `Error`. No `STRUCT_HASH`, registry, `Id`, or engine coupling — pure value ⇄
  bytes, decode fails only on a buffer/size mismatch (`UnexpectedEof`) plus
  intrinsic per-type checks. The trait is named `WaveWire` (renamed from `Wire`);
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
  (Unique/NonUnique): emits `STRUCT_HASH`, `Wire`, inherent consts
  (`SHAPE`/`HAS_VALIDATE`/`HAS_PREPROCESS`), `WaveDbStruct`, and
  for NonUnique the generated `{Name}PivotId` + `{Name}Pivot`. `#[wavedb::pivot(...)]`
  parsed/stripped → secondary-index count. `#[server]` deferred to M4 (needs `Db`).
  - **`STRUCT_HASH` uses SeaHash (pinned crate)** — portable across arch/endianness so
    client and server agree on identity; the crate is version-pinned so identity can't drift.
- **`wavedb-build`** — `generate_registry()` scans `src/`, emits the `Object` enum
  (`from_wire`/`to_wire`/`struct_hash` + a `STRUCT_HASH`-printing `Debug`) — the enum
  *is* the registry, no descriptor table. Generated code carries `#[allow(...)]` so it
  never lints the user's crate.
- **`examples/schema-smoke`** — end-to-end M1 proof: `#[wavedb]` + `build.rs` +
  `include!` → registry resolves + `Object` round-trips. (Real example; `todo-app`
  still needs M4 `#[server]`/`Db`.)
- **`wavedb-storage` foundations** — `block` (`BlockDescriptor` u40·u20·u4 packing,
  `Run`, `BlockAllocator`: best-fit alloc / coalescing free / tail truncate) and
  `directory` (linear-hashing `bucket_index`/`next_split_bucket`, `Directory`).
  - **Page `hash_of` is SeaHash** — portable across arch/endianness, so journal replay
    rebuilds `data.bin` with the same routing even when the file moves machines. Random
    per-DB seed keeps DoS resistance.

_86 tests, clippy-clean. Workspace members: wire, wire-derive, core, macros, build,
storage, schema-smoke._
