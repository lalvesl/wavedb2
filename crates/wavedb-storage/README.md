# wavedb-storage

The per-node **storage engine**: the block manager, the per-`STRUCT_HASH` page
directory (linear-hashed), the page format, dictionaries, and the journal-backed
write pipeline. This is where most of WaveDB's engineering energy lives.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md). The browser does **not** use this engine — see
> [`wavedb-wasm`](../wavedb-wasm/README.md) for the IndexedDB key→value path.

## Module map

| Module         | Responsibility                                                            |
| -------------- | ------------------------------------------------------------------------- |
| `block`        | `BlockFile` / block allocator — alloc/free/coalesce runs of 4 KiB blocks. |
| `directory`    | The per-`STRUCT_HASH` `Vec<u64>` page directory + linear hashing.         |
| `page`         | Page format + the `PageFormat` derive trait (crc32, id list, blob).       |
| `dictionary`   | Per-`STRUCT_HASH` compression dictionary + its block run.                 |
| `pipeline`     | Journal, in-memory `BTreeMap` cache, background settle + rebalance.       |
| `node_storage` | Top-level façade tying the files together for a node.                     |

---

## Two storage targets

| Target           | Backing store                        | Journal        |
| ---------------- | ------------------------------------ | -------------- |
| **Native**       | Filesystem `data.bin` (+ block runs) | `journal` file |
| **Web (wasm32)** | IndexedDB directly (key→value)       | **not needed** |

Everything in this crate describes the **native** engine. The browser owns no
physical layout — IndexedDB is already an ordered key→value store — so the WASM
build skips pages, the block manager, and the journal entirely.

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

## Per-`STRUCT_HASH` page directory

`data.bin` is **partitioned by type**: each `STRUCT_HASH` owns its own page
directory — an in-memory `Vec<u64>` where every entry is a 64-bit **page
descriptor** pointing at one homogeneous page (a run of blocks holding records of
exactly that `STRUCT_HASH`). One `Vec<u64>` per type, so unrelated types never
share a page and each page compresses against one tight dictionary.

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
serialize/deserialize over `Wire`, separately for each of the four page kinds
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
- **blob** — the `Wire`-encoded record bytes to parse.

The `PageFormat` trait also owns the **management of the `Vec<u64>` directory** —
calling the block manager to allocate/free runs and driving the `Wire`
serialize/deserialize. Page kinds:

| Kind          | What the blob holds                                              |
| ------------- | ---------------------------------------------------------------- |
| **Unique**    | The single live record per tenant at its fixed anchor address.   |
| **NonUnique** | The collection's records (timestamp-keyed).                      |
| **Pivot**     | Collection handles: `counter`, `current`/`dead` BpTree pointers. |
| **BpTree**    | B+tree index nodes — record addresses, not record bytes.         |

---

## Dictionaries

Because a page holds exactly one `STRUCT_HASH`, each type has **one dictionary**
with nothing foreign to dilute it. A dictionary is its own struct, stored in
`data.bin` in block runs handed out by the block manager and tracked by a small
**dictionary directory** — a `Vec<u64>` using the **same block descriptor** as
the page directory (`u40 start · u20 count · u4 occupation`), so allocator and
directory code is shared. Page headers reference the dictionary they were
compressed with so pages stay readable across a dictionary rebuild; a superseded
dictionary run is freed once no live page references it. Variable-length heap
values (strings/blobs) are additionally zstd-compressed. CPU is free here — there
is no join processing competing for it.

---

## Write pipeline & concurrency

```
mutation → journal append → in-memory BTreeMap<Id> cache → (client confirmed)
                                          │
                                          └─ background: settle into data.bin,
                                             then rebalance (split_next)
```

1. **Journal first.** Every insert/update/delete/save — and every block
   alloc/free — is appended to the journal before anything else. This is what
   makes the durability guarantee and prevents leaks.
2. **Cache as a `BTreeMap<Id, …>`.** Confirmed mutations live in an in-memory
   `BTreeMap` keyed by `Id`; reads serve from it directly (and, because `Id` is
   ordered by `KEY`, range/timeline reads are naturally ordered).
3. **Background settle.** A drain task writes cached pages down into `data.bin`
   at its own pace and can flush entries out of memory once persisted.
4. **Background rebalance.** Because page grow happens in place without moving
   keys, pages can get large; the rebalancer runs `split_next` off the hot path
   to keep page sizes bounded.

On startup the directories and the block allocator rebuild by **journal replay**.

---

## Operations on records

- **Unique** — `get` resolves the fixed anchor (`STRUCT_HASH · TENANT · 1 · 0`)
  in one lookup. `save` writes the new live bytes to the anchor and chains the
  previous version into history via `Metadata`.
- **NonUnique** — `insert` / `update` / `delete` each revalidate the `Pivot`'s
  `BpTree`. `delete` moves the entry from the **current** tree to the **dead**
  tree; the record bytes are never erased, keeping the timeline navigable.

---

## Reliability

- **Journal** — append-only, replayed on startup; record writes and the
  alloc/free deltas are journaled together so a mid-mutation crash can't desync
  the directory from the data.
- **Checksums** — every page carries a CRC32 verified on read.
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
