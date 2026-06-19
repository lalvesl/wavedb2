# TO DO

Clean reimplementation of WaveDB. The docs describe the **target** design; no
code exists yet. Build order, roughly bottom-up:

## Foundations (`wavedb-core` + `wavedb-macros`)

- `Id` (128-bit: `KEY u64 · TENANT u48 · FLAG 1 · SALT 15`) with accessors +
  per-shape `SALT` (Unique `0`; NonUnique/BpTree/Pivot = 15 random bits, no
  struct-hash truncation);
- `STRUCT_HASH` = ahash with a **fixed hard-coded seed** over
  `name + shape + field names + field types` (deterministic across builds);
- `Metadata` (modification chain, user, device, permission) — **no version
  field**;
- `Wire` trait + `WaveWire` derive (no serde, no `repr(C)`); see
  `docs/wire_format.md`;
- `#[wavedb]` macro: shapes `Unique` (default) / `NonUnique`; generate the
  `Pivot` + `BpTree` *types*; `PivotId` field references for nesting;
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
- _Future:_ `update_call` kind + extra per-property `BpTree`s (secondary indexes).

## Storage engine (`wavedb-storage`)

- block manager: alloc/free/coalesce/truncate runs of 4 KiB blocks, journaled;
- per-`STRUCT_HASH` `Vec<u64>` page directory; one block descriptor
  (`u40 start · u20 count · u4 occupation`) shared by pages **and** dictionary;
- `hash_of(id)` = ahash over the `u128`, seeded by a **per-DB random `[u64;4]`
  in data.bin page 0**; result feeds linear hashing;
- **linear hashing** (`index` / `split_next`), 16 KiB first page, grow-in-place +
  background `split_next`;
- `PageFormat` derive trait per page kind (Unique / NonUnique / Pivot / BpTree):
  `crc32 + STRUCT_HASH + id-list + blob`, `Wire` ser/deser;
- per-`STRUCT_HASH` dictionaries + dictionary directory (same block descriptor);
- write pipeline: journal-first → in-memory `BTreeMap<Id>` cache → background
  settle → background rebalance; journal replay on startup;
- **atomicity = the cache**: a multi-record op (record + `BpTree`) is one journal
  entry applied to the cache atomically; no separate txn manager. `Pivot` has no
  counter, so it is read-not-written on a normal NonUnique op;
- IO budget: Unique `save` = 4 IOs; NonUnique `insert`/`save`/`delete` = 7 IOs.

## Client (`wavedb`)

- `Db::connect` / `Db::open` family (native file + wasm IndexedDB);
- typed CRUD: Unique `get`/`save`; NonUnique `insert`/`update`/`delete` +
  collection walk via `Pivot`/`BpTree`. No query DSL.

## Server functions (`#[server]`) — replaces query

- `#[server]` proc-macro: server-only async body + client call binding;
- `FN_HASH` (name + arg types + return type) identity; args/return via `Wire`;
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
- cross-tenant read *path* (multi-node routing + grant enforcement) — model
  stays, serving path deferred;
- offline-first reconciliation.

## Resolved bit budgets

- **ID** = `KEY u64 + TENANT u48 + FLAG 1 + SALT 15 = 128`. No reserved bits.
- **Block descriptor** = `start u40 + count u20 + occupation u4 = 64`
  (~4 PiB/file, ~4 GiB/page, 1/16th occupation). One format for pages **and**
  dictionary.

# DOING

# DONE
