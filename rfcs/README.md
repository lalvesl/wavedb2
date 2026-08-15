# WaveDB RFCs

This directory is the **design record and progress tracker** of WaveDB: one
numbered document per idea. Where the crate READMEs describe the *target*
architecture, an RFC captures **the idea itself** — the problem it solves, the
shape of the solution, its current status, and the alternatives that lost — so a
decision can be understood (and revisited) years later without re-deriving it
from the code.

See [RFC 0000](0000-rfc-process.md) for how this process works (numbering,
statuses, the filename-marker convention).

## Current state

_A snapshot for orientation; each RFC's status header is authoritative._

- **Shipped (M1–M7, M8):** the wire codec, type identity, the platform seam, the
  full data model (anchors + `Succession` history, B+tree collections, natural
  keys), the macro & exposure system, the storage engine + journal-rooted
  recovery, the HTTP + WebSocket transport, the connection manager, live sync by
  navigation catch-up over both poll and WebSocket (including W6 reconnect —
  [0034](0034-ws-reconnect-catchup.md)) and W7 poll efficiency (idle backoff +
  command piggyback — [0035](0035-http-piggyback-and-idle-backoff.md)), the
  client `Db` + write-through cache, the wasm/IndexedDB target, and auth. RFCs
  0003–0026 are *Implemented* (except the two Partial noted below; 0014 and 0040
  are both *Deprecated* — see the next bullet).
- **Schema migration is the developer's, entirely.** Both engine-side designs —
  the 0014 hook seam and the 0040 version chain — are *Deprecated*; a changed
  struct is simply a new type ([0004](0004-struct-hash-and-schema-evolution.md))
  and moving data between two types is application code. What survived is the
  exposure registry's **compile-time** collision guard (full-`STRUCT_HASH` clash
  = error, 15-bit `type_salt` clash = warning), recorded in
  [0040](0040-schema-migration-and-version-skew-DEPRECATED.md).
- **The storage write path (2026-07-28 → 07-29):**
  [0041](0041-single-barrier-checkpoint.md) *(Implemented)* replaced the per-id
  settle — which read-modify-wrote a bucket page once per record — with a
  planned window: pages grouped per bucket in RAM, one contiguous best-fit
  allocation, **one** positioned write covering pages + dictionary + directory
  chains, then the descriptor swap. A settle round takes no barrier at all; a
  checkpoint takes one, the window's (see 0046 for how the `Commit` frame
  stopped taking a second). The per-mutation path was already one append +
  fsync and did not change.
  [0042](0042-free-space-defragmentation.md) *(Implemented)* keeps such windows
  available: a background pass relocates live pages stranded between holes to
  fresh tail blocks, so the space they vacate coalesces into the extent the next
  checkpoint's best-fit lands in. And
  [0043](0043-descriptors-in-the-commit-frame.md) *(Implemented)* moved the
  addressing state **into** the `Commit` frame — one journal append carries every
  type's descriptor vector plus the retired journal's DONE marker, so `data.bin`
  holds pages and dictionaries and nothing else, and the copy-on-write directory
  chain is gone.
  Finally [0046](0046-directory-deltas-in-the-window.md) *(Implemented)* fixed
  the one cost 0043 left behind — a frame carrying every bucket of every type
  scales with the **database**, not the change (1 MiB per checkpoint at 2 GiB,
  100 MiB at 200 GiB) — by moving the descriptor changes into the settle
  window itself: metadata for **zero** extra IOps, with the frame reduced to a
  snapshot address plus the deltas since it, compacted by a periodic full
  chunk. Because that frame is only a pointer into already-durable state it
  needs no barrier of its own — a checkpoint costs **one**, the `data.bin`
  sync, with the retirement it authorises deferred until an ordinary write's
  fsync carries the frame.
  [0047](0047-generational-journal-retirement.md) *(Implemented)* closed the
  accounting: that retirement is now disposed of by the **next** checkpoint,
  which holds the journal carrying the frame and can read its barrier count,
  rather than being chased from the write path (a lock and an `unlink` inside a
  batch) or from an idle timer (a whole barrier for housekeeping). Two journals
  on disk is the steady state — disk being the abundant resource — and no
  barrier is paid anywhere on a checkpoint's behalf.
- **Proposed next (storage & query):**
  [0048](0048-chained-addressing-log-PLANNED.md) — give each edit chunk a
  `prev` pointer so the `Commit` frame carries one address instead of the whole
  chunk list. The frame is rewritten in full every checkpoint to name state the
  previous frame already named, which makes the log's write cost quadratic in
  the compaction interval — and therefore caps that interval, forcing the
  O(database) snapshot far more often than it need be;
  [0049](0049-elastic-pages-and-load-driven-splits-PLANNED.md) — stop enforcing
  a page size. Linear hashing's split order is derived from the directory
  length and cannot be chosen, so today's per-page overflow trigger splits
  ~N/2 innocent buckets to relieve one — and never terminates for a record
  larger than the threshold. Splits become load-driven and an over-target
  bucket simply spans more blocks, which also makes a large object just a
  large page;
  [0044](0044-page-cache-PLANNED-LOW.md) — a page-granular cache so the read
  that precedes a write also serves the settle's read-modify-write;
  [0045](0045-vector-search-PLANNED.md) — nearest-neighbour search as a
  declared index kind (IVF over the existing `BpTree`, per-tenant centroids).
- **Deferred (low priority):**
  [0036](0036-offline-write-queue-PLANNED-LOW.md) — W8 offline write queue
  (slice 1, Unique offline, *shipped*; the NonUnique/durable/conflict-surface
  remainder is deferred — the near-term focus is online + small offline),
  [0037](0037-multi-node-cluster-PLANNED-LOW.md) (multi-node cluster),
  [0038](0038-argon2-and-oauth-credentials-PLANNED-LOW.md) (Argon2/OAuth),
  [0039](0039-developer-experience-PLANNED-LOW.md) (M9 dev tooling).
- **Partial seams:** [0013](0013-permissions.md) (per-record grants, gate 4),
  [0023](0023-quick-node-and-gates.md) (node gates 5–6).

## Index

### Meta
| # | Title | Status |
|---|-------|--------|
| [0000](0000-rfc-process.md) | The RFC process | Accepted |

### Vision & rules
| # | Title | Status |
|---|-------|--------|
| [0001](0001-vision-and-non-goals.md) | Vision, motivation, and non-goals | Accepted |
| [0002](0002-architectural-hard-rules.md) | Architectural hard rules | Accepted |

### Foundations
| # | Title | Status |
|---|-------|--------|
| [0003](0003-wavewire-wire-format.md) | The WaveWire wire format | Implemented |
| [0004](0004-struct-hash-and-schema-evolution.md) | STRUCT_HASH identity & schema evolution | Implemented |
| [0005](0005-composite-ids-and-bit-budgets.md) | Composite IDs and bit budgets | Implemented |
| [0006](0006-platform-seam.md) | The platform seam (native ⇄ browser) | Implemented |

### Data model
| # | Title | Status |
|---|-------|--------|
| [0007](0007-tenancy-and-data-ownership.md) | Tenancy and data ownership | Implemented |
| [0008](0008-store-trait-and-atomic-batch.md) | The Store trait and the atomic batch | Implemented |
| [0009](0009-anchors-succession-and-history.md) | Anchors, Succession, and history (DB-1) | Implemented |
| [0010](0010-metadata-and-record-envelopes.md) | Metadata and record envelopes | Implemented |
| [0011](0011-bptree-index-and-collections.md) | B+tree index, collections, and Pivots | Implemented |
| [0012](0012-natural-keys.md) | Natural keys (`#[wavedb::key]`) | Implemented |
| [0013](0013-permissions.md) | Permissions | Partial |

### Macros & exposure
| # | Title | Status |
|---|-------|--------|
| [0015](0015-wavedb-macro.md) | The `#[wavedb]` declarative macro | Implemented |
| [0016](0016-server-functions.md) | Server functions (`#[server]`) | Implemented |
| [0017](0017-exposure-registry-and-side-features.md) | The exposure registry & schema side-features | Implemented |

### Storage engine
| # | Title | Status |
|---|-------|--------|
| [0018](0018-storage-engine.md) | The storage engine | Implemented |
| [0019](0019-journal-rooted-recovery.md) | Journal-rooted recovery | Implemented |

### Transport, node & sync
| # | Title | Status |
|---|-------|--------|
| [0020](0020-net-transport-dumb-tunnel.md) | The net transport (dumb tunnel) | Implemented |
| [0021](0021-connection-manager.md) | The connection manager | Implemented |
| [0022](0022-live-sync-navigation-catchup.md) | Live sync by navigation catch-up | Implemented |
| [0023](0023-quick-node-and-gates.md) | The quick-node and enforcement gates | Partial |

### Client & targets
| # | Title | Status |
|---|-------|--------|
| [0024](0024-client-db-and-cache.md) | The client `Db` and write-through cache | Implemented |
| [0025](0025-wasm-indexeddb-target.md) | The wasm / IndexedDB target | Implemented |
| [0026](0026-auth-tokens.md) | Auth: access & refresh tokens | Implemented |

### Roadmap — in progress & planned
| # | Title | Status |
|---|-------|--------|
| [0034](0034-ws-reconnect-catchup.md) | W6: WebSocket reconnect catch-up | Implemented |
| [0035](0035-http-piggyback-and-idle-backoff.md) | W7: HTTP piggyback + idle backoff | Implemented |
| [0036](0036-offline-write-queue-PLANNED-LOW.md) | W8: Offline write queue (slice 1 shipped) | Planned (low) |
| [0037](0037-multi-node-cluster-PLANNED-LOW.md) | Multi-node cluster | Planned (low) |
| [0038](0038-argon2-and-oauth-credentials-PLANNED-LOW.md) | Argon2 & OAuth/OIDC credentials | Planned (low) |
| [0039](0039-developer-experience-PLANNED-LOW.md) | Developer experience (M9) | Planned (low) |
| [0041](0041-single-barrier-checkpoint.md) | Single-barrier checkpoint | Implemented |
| [0042](0042-free-space-defragmentation.md) | Free-space defragmentation | Implemented |
| [0043](0043-descriptors-in-the-commit-frame.md) | Descriptors in the `Commit` frame | Implemented |
| [0044](0044-page-cache-PLANNED-LOW.md) | The page cache | Planned (low) |
| [0045](0045-vector-search-PLANNED.md) | Vector search | Planned |
| [0046](0046-directory-deltas-in-the-window.md) | Directory deltas in the settle window | Implemented |
| [0047](0047-generational-journal-retirement.md) | Generational journal retirement | Implemented |
| [0048](0048-chained-addressing-log-PLANNED.md) | The addressing log as a chain | Planned |
| [0049](0049-elastic-pages-and-load-driven-splits-PLANNED.md) | Elastic pages and load-driven splits | Planned |

### Deprecated / superseded
| # | Title | Superseded by |
|---|-------|---------------|
| [0014](0014-schema-evolution-hooks-DEPRECATED.md) | Schema-evolution lookup hooks | [0040](0040-schema-migration-and-version-skew-DEPRECATED.md) |
| [0040](0040-schema-migration-and-version-skew-DEPRECATED.md) | Schema migration & node/client version skew | *dropped* — migration is the developer's ([0004](0004-struct-hash-and-schema-evolution.md)) |
| [0027](0027-doubly-linked-modification-chain-DEPRECATED.md) | Doubly-linked modification chain | [0009](0009-anchors-succession-and-history.md) |
| [0028](0028-journal-commit-cursor-sync-DEPRECATED.md) | Journal commit-cursor sync | [0022](0022-live-sync-navigation-catchup.md) |
| [0029](0029-bloom-filter-screen-sync-DEPRECATED.md) | Bloom-filter screen-sync | [0022](0022-live-sync-navigation-catchup.md) |
| [0030](0030-superblock-pointer-checkpoint-DEPRECATED.md) | Superblock-pointer checkpoint | [0019](0019-journal-rooted-recovery.md) |
| [0031](0031-node-per-page-bptree-DEPRECATED.md) | One-node-per-page B+tree format | [0011](0011-bptree-index-and-collections.md) |
| [0032](0032-node-side-poll-buffer-DEPRECATED.md) | Node-side stateful poll buffer | [0022](0022-live-sync-navigation-catchup.md) |
| [0033](0033-cold-history-slow-node-tier-DEPRECATED.md) | Cold/history slow-node tier | removed |

## Status vocabulary

Delivery status is a header field and, for the non-baseline states, also a
**filename marker** (like `DEPRECATED`) so a directory listing *is* the roadmap:

| Status | Filename marker | Meaning |
|--------|-----------------|---------|
| **Accepted** | — | The decision stands; may be a policy rather than code. |
| **Implemented** | — | Landed and proven; the landing date is in the RFC's status header. |
| **Partial** | — | Core built, a seam remains; the RFC names it. |
| **In progress** | `WIP` | Actively being built now. |
| **Planned** | `PLANNED` | Accepted, will be built, not started. |
| **Planned (low)** | `PLANNED-LOW` | Deferred; someday. |
| **Deprecated** | `DEPRECATED` | Replaced/rejected; body points at what replaced it. |

Changing status is a **rename** (the number and history stay); a deprecated or
superseded idea keeps its file so the dead idea stays findable. The number is
never reused.
