# wavedb-storage

The per-node **storage engine**: the block manager, the per-`STRUCT_HASH` page
directory (linear-hashed), the page format, dictionaries, and the journal-backed
write pipeline. This is where most of WaveDB's engineering energy lives.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md). The browser does **not** use this engine — see
> [`wavedb-wasm`](../wavedb-wasm/README.md) for the IndexedDB key→value path.

## Module map

| Module         | Responsibility                                                                                                                                                                        |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `block`        | `BlockFile` / block allocator — alloc/free/coalesce runs of 4 KiB blocks.                                                                                                             |
| `directory`    | The per-`STRUCT_HASH` `Vec<u64>` page directory + linear hashing.                                                                                                                     |
| `page`         | Page format + the `PageFormat` derive trait (crc32, id list, blob).                                                                                                                   |
| `dictionary`   | Per-`STRUCT_HASH` compression dictionary + its block run.                                                                                                                             |
| `pipeline`     | Journal, in-memory `BTreeMap` cache, background settle + rebalance.                                                                                                                   |
| `node_storage` | `NodeStorage` — the node's **authoritative** engine: owns the files + all sub-modules, runs `Pivot`/`BpTree` + page writes. Reached over the network; not the client's local `Store`. |

---

## Where this crate runs

This crate is the **node's authoritative engine** — native only: filesystem
`data.bin` (block runs) + `journal`. It provides the native **`PageStore`** — the
disk-optimised `Store` backend (`get` + atomic `apply`) — that the `Store`-generic
`Pivot`/`BpTree` contracts in [`wavedb-core`](../wavedb-core/README.md#index-contracts--pivot-bptree-indexkey)
run over. The index logic itself is portable; pages, blocks, journal, and the
allocator are this crate's internals, hidden behind the `Store` seam.

It is **not** the client-side local store. The client's local key→value cache
(`Store` trait) is a separate, lighter thing: a file kv on native, IndexedDB on
web ([`wavedb-wasm`](../wavedb-wasm/README.md)) — no pages, no journal. See
[`wavedb-core` § the `Store` trait](../wavedb-core/README.md#store--the-local-backend-trait).

---

## Block manager

`data.bin` is an **array of fixed 4 KiB blocks**. The block manager owns it:

- **Allocate** a contiguous run of `n` blocks for a new or growing page,
  returning its start block.
- **Free** a run when a page is relocated, emptied, or its type is dropped.
- **Coalesce** adjacent free extents so large pages always have somewhere to
  land, and **truncate** the tail when trailing blocks are free.

Free space is tracked as **extents** indexed by position (to coalesce on free)
and by size (best-fit, to satisfy "give me `n` contiguous blocks"). The allocator
is a pure in-memory structure; durability is the pipeline's job — **every alloc
and every free is appended to the journal** so a crash can replay them and never
leak or lose blocks.

---

## On-disk metadata — the superblock & the WaveWire rule

### Block 0: the superblock

The first `RESERVED_BLOCKS` (currently 1) blocks of `data.bin` are engine
metadata — the allocator never hands them out. Block 0 is the **superblock**:
it stamps the file as a WaveDB data file and carries the per-database facts.
Layout (little-endian, zero-padded to the block):

| Offset | Field                                | Size    | Meaning                                                                              |
| ------ | ------------------------------------ | ------- | ------------------------------------------------------------------------------------ |
| 0      | magic                                | 8 B     | `WAVEDBIN` — "this is a WaveDB data file". Mismatch ⇒ `StorageError::BadMagic`.      |
| 8      | `to_wire_checked(SuperblockBody)`    | 40 B    | `[crc32 (u32 LE)][wire]` of `{ version: u32, seed: [u64; 4] }`.                      |

The body is the **checked wire encoding** of `SuperblockBody` — the format
version (bumped on any incompatible change ⇒ `BadVersion` if it mismatches) and
the per-database random SeaHash seed that `hash_of` routes every record with. A
crc failure (including a pre-v2 file) surfaces as `Corrupt("superblock")`.

A fresh file mints a random seed and persists the superblock before anything
else; opening an existing file validates magic + version and loads the seed.
Keeping the seed in block 0 is what lets a `data.bin` rebuilt by journal replay
on another machine route every `Id` into the same bucket.

### One layout language: WaveWire (+ checked framing)

Engine metadata is described with **the `WaveWire` codec only** — the same
layout language records use. No structure gets its own bespoke byte format:

- **superblock body** (version + seed) — a plain `WaveWire` struct behind the
  magic. The 8-byte magic itself stays a raw prefix *outside* the wire payload:
  it must be checkable before any decode is attempted;
- **block descriptor** (`start u40 · count u20 · occupation u4`, exact-packed
  into one `u64` — see the table below) — wire-encodes as that `u64`;
- **page / dictionary directories** (`Vec<u64>` of descriptors) — the standard
  `WaveWire` `Vec` encoding;
- **per-page metadata** (the header: `struct_hash`, id list, blob layout) — a
  `WaveWire` struct.

Where a structure must survive disk corruption — **pages, journal frames** —
it uses the wire crate's [`validation` feature](../wavedb-wire/README.md#feature-validation--crc32-framed-encoding):
`to_wire_checked` / `from_wire_checked` frame the payload as
`[crc32 (4 B LE)][wire bytes]`, so integrity verification is the codec's job —
not a hand-patched CRC slot re-implemented in every format. A corrupted page
surfaces as the wire `CrcMismatch` before any decode runs.

> **Status.** The superblock body and journal frames are on checked `WaveWire`
> (`FORMAT_VERSION` 2). `SlotPage` still hand-rolls its layout (a CRC
> placeholder patched in `to_bytes`); moving it over is another
> `FORMAT_VERSION` bump, absorbed by the journal-replay rebuild — no migration
> machinery.

---

## Per-`STRUCT_HASH` page directory

`data.bin` is **partitioned by type**: each `STRUCT_HASH` owns its own page
directory — an in-memory `Vec<u64>` where every entry is a 64-bit **page
descriptor** pointing at one homogeneous page (a run of blocks holding records of
exactly that `STRUCT_HASH`). One `Vec<u64>` per type, so unrelated types never
share a page and each page compresses against one tight dictionary.

> **"bucket" = "page", 1:1.** A _bucket_ is one directory slot — the logical
> hash-table slot records hash into (the linear-hashing term). A _page_ is the
> physical run of blocks that slot points to. Each directory entry is exactly one
> page, so the two words name the same thing: "bucket" stresses its role in the
> hash table, "page" its on-disk form.

### Block descriptor (`u64`) — one format everywhere

Pages (for every `STRUCT_HASH`) **and** dictionary runs share a single 64-bit
descriptor — there is no second format:

| Bits   | Field         | Width | Meaning                                                         |
| ------ | ------------- | ----- | --------------------------------------------------------------- |
| 63..24 | `start_block` | `u40` | First 4 KiB block in `data.bin` (2⁴⁰ × 4 KiB ≈ **4 PiB**/file). |
| 23..4  | `block_count` | `u20` | Contiguous blocks (1 ≤ n ≤ 2²⁰ ⇒ ≤ **~4 GiB** per page/run).    |
| 3..0   | `occupation`  | `u4`  | Coarse fill gauge in 1/16ths (0 = empty, 15 = full).            |

`40 + 20 + 4 = 64` — exact fit. `occupation` is a cached summary the directory
can read **without touching the page** — enough to decide "this page must grow /
split" from the directory alone.

### The `Id` hash

`hash_of(id)` is **SeaHash over the 16 little-endian bytes of the `Id`** — fast and
DoS-resistant. It is seeded with a **per-database random `[u64; 4]` seed persisted
in the first page of `data.bin`** (page 0), read once at startup. The seed is
per-database (not the fixed `STRUCT_HASH` seed): each node rebuilds its own
directory consistently, and an attacker can't precompute bucket collisions. SeaHash
is portable across architectures and endianness, so a `data.bin` rebuilt by journal
replay on another machine routes every record to the same bucket. The resulting
`u64` feeds the linear-hashing reduction below.

### Linear hashing (not `%`)

Addressing uses **linear hashing**, not `hash % len`. The directory starts as one
page (~16 KiB) and grows **one bucket at a time** when a fill warning trips, so a
grow rehashes only a single bucket — never the whole type.

```rust
// Which bucket does this id land in?
pub fn index(&self, id: u128) -> usize {
    let m = self.dir.len() as u64;
    let level = m.ilog2();
    let base = 1u64 << level;
    let s = m - base;
    let h = hash_of(id);
    let mut b = h & (base - 1);
    if b < s {
        b = h & ((base << 1) - 1);
    }
    b as usize
}

// Split the next bucket in round-robin order; append one new bucket.
pub fn split_next(&mut self, file: &BlockFile) -> StorageResult<()> {
    let m = self.dir.len() as u64;
    let level = m.ilog2();
    let s = (m - (1u64 << level)) as usize; // bucket to split

    let desc = self.dir[s];
    let bucket = self.read_bucket(file, desc)?;
    // Partition by bit `level`: 0 stays in s, 1 moves to the new bucket.
    let mut keep = SlotPage::new(self.struct_hash);
    let mut moved = SlotPage::new(self.struct_hash);
    for (id, entry) in bucket.entries {
        if (hash_of(id) >> level) & 1 == 0 {
            keep.upsert_entry(id, entry);
        } else {
            moved.upsert_entry(id, entry);
        }
    }
    let keep_desc = self.place_if_nonempty(file, &keep)?;
    let moved_desc = self.place_if_nonempty(file, &moved)?;
    if desc.is_allocated() {
        file.free(desc.run());
    }
    self.dir[s] = keep_desc;
    self.dir.push(moved_desc); // new bucket at index M (= s + base)
    Ok(())
}
```

### Two kinds of growth

| Growth           | Trigger                                   | Cost                                                                                                                |
| ---------------- | ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| **Page grow**    | a bucket's `occupation` crosses the limit | allocate a larger run, copy the page, free the old run, rewrite **one** descriptor. No keys move.                   |
| **Bucket split** | the warning limit trips                   | `split_next` splits one bucket by the next hash bit and appends one directory slot. Scoped to one type, one bucket. |

**Page grow is the cheap, common case** — relocate one contiguous run to a bigger
hole, patch one pointer. Growing the page in place lets writes proceed **without
rebalancing on the hot path**; a background task runs `split_next` to keep pages
from getting too large (see _Write pipeline_).

For safety, the order is: allocate the larger run → write the updated page →
**then** rewrite the directory entry. The directory only ever points at a fully
written page.

---

## Page format

A page is **homogeneous** — records of exactly one `STRUCT_HASH`. The
`#[wavedb]` macro emits, via a `PageFormat` derive trait, the layout and the
serialize/deserialize over `WaveWire`, separately for each of the four page kinds
(`Unique`, `NonUnique`, `Pivot`, `BpTree`) so each gets its own optimal
dictionary and compression.

```
┌──────────────────────────────────────────────────────────┐
│ crc32 │ STRUCT_HASH (u64)                                  │
│ ──────────────────────────────────────────────────────    │
│ id list: [ (Id: u128, offset, size) … ]   ← dynamic sizes  │
│ ──────────────────────────────────────────────────────    │
│ blob: [ record bytes … ]                  ← Wire-encoded   │
└──────────────────────────────────────────────────────────┘
```

- **`crc32`** — verified on read.
- **`STRUCT_HASH`** — present in every page kind; identifies the type and selects
  the dictionary.
- **id list** — the `u128` IDs with each record's dynamic size and position in the
  blob. Present for all four kinds (`Unique`, `NonUnique`, `Pivot`, `BpTree`).
- **blob** — the `WaveWire`-encoded record bytes to parse.

The `PageFormat` trait also owns the **management of the `Vec<u64>` directory** —
calling the block manager to allocate/free runs and driving the `WaveWire`
serialize/deserialize. Page kinds:

| Kind          | What the blob holds                                                |
| ------------- | ------------------------------------------------------------------ |
| **Unique**    | The single live record per tenant at its fixed anchor address.     |
| **NonUnique** | The collection's records (timestamp-keyed).                        |
| **Pivot**     | Collection handles: `current`/`dead` BpTree pointers (no counter). |
| **BpTree**    | 32 KiB page; one B+tree node per page; 18-byte entries.            |

### BpTree page layout — 32 KiB, one node per page

BpTree pages are **32 KiB** (8 × 4 KiB blocks). Each page holds exactly **one**
B+tree node (either an internal node or a leaf node). Both node kinds use the
same 18-byte entry format — no special-casing:

```
entry = [ key: [u8; 8] ][ LocalId: 10 bytes ]
```

- **Internal node entry**: `LocalId` is the child BpTree page pointer.
- **Leaf node entry**: `LocalId` is the NonUnique record pointer.

All `LocalId`s are inflated to full `Id` via `local_id.to_id(tenant)` on read —
2–3 CPU cycles, never disk.

```
┌──────────────────────────────────────────────────────┐  0
│  HEADER  (20 bytes)                                  │
│    crc32 (4) · STRUCT_HASH (8) · kind (u8)           │
│    num_entries (u16) · reserved (5)                  │
├──────────────────────────────────────────────────────┤  20
│                                                      │
│  ENTRIES  (18 bytes each, tightly packed)            │
│    [ key: [u8; 8] | child/record: LocalId (10 B) ]  │
│    …                                                 │
│                                                      │
└──────────────────────────────────────────────────────┘  32 768
```

#### Capacity and tree height

```
usable  = 32 768 − 20  = 32 748 bytes
entries = 32 748 / 18  ≈  1 819 per page
```

Tree height in **page reads** (8-byte `CREATED_AT` keys):

| Records  | Page reads |
| -------- | ---------- |
| ≤ 1 819  | 1          |
| ≤ 3.31 M | 2          |
| ≤ 6.03 B | 3          |

Prior design (4 KiB page, 226-entry nodes, `LocalId` pointer per entry):

| Records | Page reads (old) | Page reads (new) |
| ------- | ---------------- | ---------------- |
| 1 M     | 4                | **2**            |
| 1 B     | 5                | **3**            |
| 6 B     | 5                | **3**            |

#### Page split

Triggered when an insert would push a node past 1 819 entries (page full).

**Step 1 — split the full node.**

```
page_X (FULL — 1 819 entries):
  [ e0, e1, … e908 | e909, e910, … e1818 ]
                   ↑
              median = e909

→ allocate page_Y (new LocalId)
→ page_X keeps  [ e0  … e908 ]   (~50%)
→ page_Y gets   [ e909 … e1818 ] (~50%)
```

**Step 2 — push median key up to parent.**

Insert `(e909.key, page_Y_localid)` into the parent internal node, immediately
to the right of the entry that pointed at `page_X`:

```
BEFORE parent: [ … | keyA → page_X | … ]
AFTER  parent: [ … | keyA → page_X | e909.key → page_Y | … ]
```

**Step 3 — cascade if parent is also full.**

The parent insert from Step 2 may itself overflow → apply Step 1–2 on the
parent, recursing up the ancestor path.

**Step 4 — root split (special case).**

If the root page overflows there is no parent to absorb the pushed key.
Instead:

```
old root (FULL):  [ e0 … e1818 ]

→ allocate page_L, page_R  (two new LocalIds)
→ page_L ← left  half  [ e0   … e908  ]
→ page_R ← right half  [ e909 … e1818 ]
→ allocate new root page_ROOT with one entry:
      page_ROOT: [ e909.key → page_R ]
      (implicit left child = page_L, stored as the "less-than" pointer)
→ Pivot.current (or .dead / secondary root) ← page_ROOT LocalId
```

The tree grows one level. **All allocated pages and the updated `Pivot` are
written in a single journal entry** — crash before the entry is committed leaves
the tree unchanged; crash after is a complete, consistent state.

#### Merge on delete

When `remove` makes a node fall below **25% fill** (≈ 455 entries), check the
adjacent sibling:

- **Sibling + current ≤ 75% full** (≤ 1 364 entries): **merge** — copy all
  entries into the sibling, free this page, remove the separator key from the
  parent. Parent may in turn underflow → recurse.
- **Sibling too full to absorb**: **redistribute** — steal entries from the
  sibling until both sit near 50%, update the separator key in the parent.

Both paths write all changed pages in a **single journal entry**. The `Pivot`
is updated only if the root page is freed (tree becomes empty or shrinks to one
level).

---

## Dictionaries

Because a page holds exactly one `STRUCT_HASH`, each type has **one dictionary**
with nothing foreign to dilute it.

### Raw-content dictionary (no trainer)

zstd accepts **any bytes** as a dictionary, not just a trained `ZDICT` — so the
dictionary is simply a **growing buffer of representative records** for that
`STRUCT_HASH`. No training pass: as inserts arrive, sample bytes are appended to
the buffer and it grows. (`zstd::dict::{EncoderDictionary, DecoderDictionary}`
take a raw byte slice.) Simpler than a trainer and naturally incremental.

The dictionary is its own struct, stored in `data.bin` in block runs handed out
by the block manager and tracked by a small **dictionary directory** — a
`Vec<u64>` using the **same block descriptor** as the page directory
(`u40 start · u20 count · u4 occupation`), so allocator and directory code is
shared.

### Versioning (the one rule)

A page compressed against dictionary state X **must** be decompressed against the
exact same bytes — you never mutate the dictionary under existing pages. So the
buffer carries a `dict_version`; the growing happens for **new** writes (they
bind the latest version), while old pages keep the `dict_version` stamped in
their header and stay readable. When the buffer has grown enough, a background
task recompresses cold pages to the newer version; a superseded dictionary run is
freed once no live page references it.

Variable-length heap values (strings/blobs) are additionally zstd-compressed.
CPU is free here — there is no join processing competing for it.

---

## Write pipeline & concurrency

```
mutation → journal append → in-memory BTreeMap<Id> cache → (client confirmed)
                                          │
                                          └─ background: settle into data.bin,
                                             then rebalance (split_next)
```

1. **Journal first.** Every insert/save/remove — and every block alloc/free — is
   appended to the journal before anything else. This is what makes the durability
   guarantee and prevents leaks.
2. **Cache as a `BTreeMap<Id, …>`.** Confirmed mutations live in an in-memory
   `BTreeMap` keyed by `Id`; reads serve from it directly (and, because `Id` is
   ordered by `KEY`, range/timeline reads are naturally ordered).
3. **Background settle.** A drain task writes cached pages down into `data.bin`
   at its own pace and can flush entries out of memory once persisted.
4. **Background rebalance.** Because page grow happens in place without moving
   keys, pages can get large; the rebalancer runs `split_next` off the hot path
   to keep page sizes bounded.

On startup the directories and the block allocator rebuild by **journal replay**.

### Atomicity is the in-memory cache

A single operation can touch several records (a NonUnique insert writes the data
record **and** a `BpTree` node). These are committed as **one journal entry** and
applied to the in-memory cache **atomically** — so readers see all-or-nothing, and
a crash before the background settle replays the journal back into the cache. No
separate transaction manager: the journal entry + cache update _is_ the atomic
unit. (Multi-write **server-function** transactions — several operations as one
unit — remain a separate, later concern.)

### IO cost per operation

Counting journal + `data.bin` IOs (the cache absorbs repeats; cold case shown):

| Operation                         | IOs      | Breakdown                                                                                                                                                                                           |
| --------------------------------- | -------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Unique `save`**                 | 4        | journal data entry · read the page · write the page · allocation delta in journal                                                                                                                   |
| **NonUnique `save`** (update)     | `7 + 2N` | journal · read old page (old keys) · read `Pivot` (roots via `Metadata.pivot`) · write record page · reindex `current` **and** each of `N` secondary trees (read+write per tree) · allocation delta |
| **NonUnique `insert` / `remove`** | `7 + 2N` | journal · read 3 pages (record, `Pivot`, `BpTree`) · write record + reindex `current` **and** each of `N` secondary trees · allocation delta                                                        |

A NonUnique **`save`** is no longer a free in-place rewrite: update **force-reindexes
every live tree** — the `current` `BpTree` _and_ every `#[wavedb::pivot(...)]`
secondary — removing the record's old entries and reinserting for the new version,
so it costs **insert-class** IO that scales with the secondary-index count `N`. It
reaches the roots through **`Metadata.pivot`**; the `Pivot` itself is still
**read, not written** unless a `BpTree` root moves (no counter). The **`dead`** tree
is **not** touched on update — history is the `Metadata` modification chain — so
only **`remove`** writes `dead`. The record's identity `Id` (insert anchor) stays
stable so references don't break; the trees re-establish the live version against
it. All allocation deltas ride in a single journal write.

---

## Operations on records

- **Unique** — `get` resolves the fixed anchor (`STRUCT_HASH · TENANT · 1 · 0`)
  in one lookup. `save` writes the new live bytes to the anchor and chains the
  previous version into history via `Metadata`.
- **NonUnique** — a collection's `Pivot` is created **explicitly** (one per tenant
  per definition) and its `PivotId` stored by the holder; never auto-created. A
  record's **identity `Id` is fixed at `insert`** (stable anchor for references),
  so:
  - **`save`** (update) **force-reindexes every live tree** — the `current`
    `BpTree` _and_ every secondary — removing the record's old entries and
    reinserting for the new version. It reaches the roots through `Metadata.pivot`.
    The **`dead`** tree is **not** touched: the previous version is retained and
    linked by the `Metadata` chain (`old_modification_id` ↔ `new_modification_id`);
  - **`insert`** adds the record to the `current` `BpTree` (and every secondary) and
    stamps `Metadata.pivot`; **`remove`** moves it to the **dead** tree — the only
    op that writes `dead`. All go through the `Pivot`.
    The record bytes are never erased, keeping the timeline navigable.

### `BpTree` stores only `Id`s — reads are two-phase

A `BpTree` holds **`Id`s, never record bytes**. Its nodes are themselves stored
as pages (the `BpTree` `STRUCT_HASH`) and linked by `Id`, so a search walks the
tree node-by-node and **returns a set/range of matching record `Id`s** — nothing
more. Resolving those `Id`s to actual records is a **separate step**: each `Id` is
fetched through its type's page directory (a normal record `get`). So a NonUnique
read is:

1. **Index search** — walk the `Pivot`'s `current` `BpTree` (ordered by
   `CREATED_AT`) → matching `Id`s;
2. **Record fetch** — resolve each `Id` to its bytes via the per-`STRUCT_HASH`
   directory.

Keeping the tree ID-only is what makes it small and shallow, and lets the two
phases be scheduled independently (e.g. fetch only the page of `Id`s the caller
actually wants).

### History & space

There is **no cold tier** (the slow-node was removed — not part of the design for
now). History is first-class and **never erased**: superseded versions and
deleted-record bytes stay in `data.bin`, reachable via the modification chain and
the `dead` `BpTree`. So a node's `data.bin` **grows unbounded** with history — and
that is **accepted for now**. History pruning / compaction / a separate archive
tier is left for later; nothing reclaims old versions today.

---

## Reliability

- **Journal** — append-only, replayed on startup; record writes and the
  alloc/free deltas are journaled together so a mid-mutation crash can't desync
  the directory from the data.
- **Checksums** — every page carries a CRC32 verified on read (converging on
  the wire `validation` framing — see _the WaveWire rule_ above).
- **Locks** — ID-scoped, in process memory.

---

## Configuration (initial)

| Parameter             | Description                                        | Default        |
| --------------------- | -------------------------------------------------- | -------------- |
| `block_size`          | Allocation unit; a page is `block_count` of these  | 4 KiB          |
| `first_page_size`     | Size of the first page / directory bucket          | 16 KiB         |
| `max_blocks_per_page` | Ceiling on one page's run (`block_count` is `u20`) | 2²⁰−1 (~4 GiB) |
| `warning_limit`       | Fill level that triggers a background `split_next` | tunable        |
| `cache_budget`        | RAM budget for the `BTreeMap` write/read cache     | tunable        |
