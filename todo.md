# TO DO

Clean reimplementation of WaveDB. The docs describe the **target** design; no
code exists yet. Build order, roughly bottom-up:

## Foundations (`wavedb-core` + `wavedb-macros`)

- `Id` (128-bit: `KEY · TENANT · FLAG · SALT`) with accessors; decide the 8
  reserved low bits (see bit-budget note below);
- `STRUCT_HASH` const computed from `name + shape + field names + field types`;
- `Metadata` (modification chain, user, device, permission) — **no version
  field**;
- `Wire` trait + `WaveWire` derive (no serde, no `repr(C)`); see
  `docs/wire_format.md`;
- `#[wavedb]` macro: shapes `Unique` (default) / `NonUnique`; auto-generate
  `Pivot` + `BpTree`; `PivotId` field references for nesting;
- `declare_objects!` registry keyed by `STRUCT_HASH`;
- migration between struct hashes (`migrate_from`/`migrate_rollback` + chain
  traits), lazy upgrade on read;
- permissions: tenant-only / public / tenant-list (group deferred).

## Storage engine (`wavedb-storage`)

- block manager: alloc/free/coalesce/truncate runs of 4 KiB blocks, journaled;
- per-`STRUCT_HASH` `Vec<u64>` page directory; page descriptor packing
  (`u46 start · u12 count · u6 occupation`);
- **linear hashing** (`index` / `split_next`), 16 KiB first page, grow-in-place +
  background `split_next`;
- `PageFormat` derive trait per page kind (Unique / NonUnique / Pivot / BpTree):
  `crc32 + STRUCT_HASH + id-list + blob`, `Wire` ser/deser;
- per-`STRUCT_HASH` dictionaries + dictionary directory (`u48 pos · u16 count`);
- write pipeline: journal-first → in-memory `BTreeMap<Id>` cache → background
  settle → background rebalance; journal replay on startup.

## Client (`wavedb`)

- `Db::connect` / `Db::open` family (native file + wasm IndexedDB);
- typed CRUD: Unique `get`/`save`; NonUnique `insert`/`update`/`delete` via
  `Pivot`/`BpTree`; typed `Expr` queries.

## Nodes & transport (`wavedb-quick-node`, `wavedb-net`)

- node-side enforcement gates (header → decode → validate → preprocess);
- tenant write-ownership ring + gossip + replication + routing/failover;
- WS / HTTP transports; Bloom screen-sync.

## Browser (`wavedb-wasm`)

- IndexedDB key→value adapter (no pages, no journal); same typed `Db`.

## Deferred

- **Slow-node / cold history tier** — out of scope for now;
- **Permission groups**;
- `STRUCT_HASH`-grained write-ownership (tenant-only for now);
- offline-first reconciliation;
- richer async server-side functions (DB-access hooks) for full-stack backends.

## Open questions / to confirm

- **ID bit budget:** `KEY u64 + TENANT u48 + FLAG 1 + SALT 7 = 120` bits; 8 low
  bits currently reserved. Confirm (extend `SALT`? add a field?).
- **Page descriptor:** `u48 + u12 + u6 = 66` overflows `u64`; `start_block`
  trimmed to `u46` (still ~256 PiB). Confirm or re-balance widths.

# DOING

# DONE
