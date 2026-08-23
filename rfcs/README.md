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
  Alongside them, [0049](0049-elastic-pages-and-load-driven-splits.md)
  *(Implemented)* stopped the engine enforcing a page size. Linear hashing's
  split order is derived from the directory length and cannot be aimed, so a
  per-page overflow trigger split ~N/2 innocent buckets to relieve one — and
  never terminated at all for a record larger than the threshold, since splits
  distribute whole records: every touching round burned its full 64-split
  budget forever. The trigger now asks about the bucket whose turn it is, so a
  split only happens where it relieves something, an over-target bucket simply
  spans more blocks until its turn, and a large object is just a large page.
  And [0048](0048-chained-addressing-log.md) *(Implemented)* took the last
  recurring cost out of the frame: it was rewritten in full at every checkpoint
  to name state the previous frame already named, so a compaction cycle of N
  checkpoints cost 4N² journal bytes — quadratic in the interval, and therefore
  a ceiling on it, forcing the O(database) snapshot far more often than
  necessary. Each chunk now names the one before it, the frame names only the
  head, and it is a fixed 16 bytes however long the log grows.
- **The default, settled 2026-07-31 —
  [0054](0054-no-duplication-by-default.md):** a record lives at its **anchor and
  nowhere else**. A collection's two instant-keyed chains — **recency** and the
  **removal log** — are the same shape, ids and nothing else (`Chain<()>`; a
  `SecKey` already carries the anchor), answering "what changed" and "what died".
  One segment read gives membership and order; each record is resolved at the
  address it always had. `all()` stays recency-ordered on purpose: a save moving
  a record to the front is the feature, and a caller wanting a stable
  domain order declares a `#[wavedb::list]` for it.
  `#[wavedb::list(...)]` is the **only** opt-in to duplication: 1 copy by
  default, `1 + K` with K lists. It opened as `layout = anchored`, a declared
  alternative, and the knob was built and deleted the same day — "not anchored"
  is not a state a record can be in (history and the dead log resolve it there),
  so the knob only ever controlled whether there was an *extra* copy, which is
  what a list already controls. And since `Chain<P>` was already payload-generic
  and the removal log was already `Chain<()>`, the whole model is one type
  parameter, not a second engine.
- **The structure it rides on (storage & query):**
  [0050](0050-clustered-record-chains.md) — a collection's records are **additionally** stored inline in a chain of
  segments ordered by the live version's **authoring instant**, so a scan costs one
  read per segment instead of one page read *and* one zstd decompression per record,
  a range keeps its logarithmic descent through the chain's sparse index, and — since
  that is exactly the key `recency` uses — **the `recency` log disappears into the
  chain**. (As proposed the chain held the records inline, a byte-identical derived
  duplicate; [0054](0054-no-duplication-by-default.md) kept the merge and dropped the
  payload — the chain that survives is `recency` itself, ids only. Records inline is
  now what a declared list is.) The record at its anchor stays put and
  stays authoritative, so every computed address, history walk and `Expect` guard is
  untouched. No chain stores a pointer
  *to* a record — position is derived from the record's own key — so splits are free of
  consequences, and because a split can always give its new id to the *interior* side,
  a chain's `head` and `tail` ids are permanent and a growing chain never rewrites the
  `Pivot`. The `current` B+tree disappears (the chain is the membership set, liveness
  moves into the anchor's `Metadata`), `dead` stays as a reference-only log chain in a
  lane of its own with **no index at all**, since nothing ever *searches* it (post-0054
  `recency` is that same shape and has its own lane too — an ~18-byte id entry and a
  segment of whole records are different content, and a per-type zstd dictionary can
  only model one of them well), and a
  dense B+tree exists only where the developer declares one. The cost is storage,
  duplicated write bytes, and `all()` changing from insertion to modification order.
  **Phases 1–7b are implemented**, and phase 8 ("compaction") was dissolved on
  2026-07-31 once it turned out to name two unrelated things: the chain already
  rebalances synchronously on every removal (phase 3b), leaving only the
  sparse index's missing merge — **taken as accepted debt**, since that index
  holds one entry per *segment* and a drained version of it is still two
  levels, and written up as
  [0055](0055-sparse-index-merge-PLANNED-LOW.md) so the bound is on the
  record — and removal-log retention, which is a client-liveness policy and
  got moved out to its own planning. The structure
  (`index/{segment,sparse,sparse_write,chain,chain_remove}.rs`): locate, insert
  with a 50/50 split at 2N, remove with a merge at N/2 or a redistribute when
  folding would breach the band, all as one batch. Liveness on the record
  (`Metadata.removed`). The `Pivot` carrying the chain roots, with the `current`,
  `recency` and `dead` B+trees now **deleted** — they were written alongside the
  chain for one phase so a test could assert the two agreed entry for entry and
  byte for byte, then retired, taking `Collection::search` (the `CREATED_AT`
  range) with them: zero callers, and a contract already false for
  `#[wavedb::key]` types, whose anchor is a content hash rather than an instant.
  Both reads come off the chain: `all()` and the wire `All` walk it back from the
  tail (post-0054 resolving each entry at its anchor — one segment read still gives
  membership and order); and reconnect catch-up (`Changes`)
  scans the chain's and the removal log's tails past the client's cursor, stopping
  at the first segment that reaches it — so a caught-up client pays three reads for
  "nothing new" whatever the collection's size, and a client behind pays segment
  reads rather than one random read per change. Plus `page = N`, the
  developer-declared segment capacity, folded into the identity so a chain is only
  ever laid out one way. Phase 7b then landed 0051 in full, and phase 8 dissolved;
  [0051](0051-ordered-record-lists.md) — **landed 2026-07-31** as 0050 phase 7b,
  built on it, and the repair for the one thing it gives up: a declared property materialises a *second* chain of
  the same records, kept sorted at write time (affordable because K extra segment
  rewrites still cost one barrier), with a **sparse** index above it — one entry
  per segment instead of per record, so the descent is two or three nodes **cold**
  (nothing is resident: 0053 forbids pinning in a multi-tenant engine, so the
  guarantee is bounded size, never residency). Ordered and range reads cost one
  dense read per segment of hits instead of one random read per hit; the price is
  `(K+1)` copies of every record on disk — the anchor plus K declared lists; it was
  `(K+2)` until 0054 emptied the built-in chain — accepted deliberately, and after
  0054 it is the *only* duplication there is. Its **wire commands landed
  2026-08-01** — `Command::Listed` (bounded: `(pivot, index, offset, limit)`) and
  `Command::ListLen` answering a new `Reply::Count` — closing the gap where an app
  could declare a list, pay a full record copy per save for it, and still not
  render a page without wrapping it in a `#[server]` fn. They needed none of the
  streaming-frame work `search_by` waits on, because a page is bounded by the
  caller's own `limit`; the typed surface gained `listed_page` so that limit is
  reachable, and the unbounded `listed` pages over it at a fixed chunk;
  [0052](0052-segment-size-as-the-pagination-unit.md) — the developer
  declares a chain's capacity as a **minimum** N, normally the page size the UI
  renders (undeclared: **16** for record chains, **256** for the removal log); a
  segment holds N…2N records, splits 50/50 at 2N with the endpoint keeping its own
  id, and **merges at N/2** — which keeps an insert to one segment rewrite where an
  exact size would cascade, and stops a chain decaying into near-empty segments as
  saves relocate their records to the growth end. A rendered page is one
  segment read, two when the window straddles a boundary — and the first is almost
  certainly still cached from the previous page. Exactness is not the goal anyway:
  `search` is an async iterable, so a tick yields whatever the segment held and the
  row count belongs to the layer above, filters included. The sparse index carries
  element counts (leaf and subtree), making it an order-statistic tree — "jump to
  page k" is one descent regardless of k, and an unfiltered pager's "of M" is the
  root's sum. Costs are quoted **cold**: WaveDB is multi-tenant, so nothing may be
  pinned in RAM. **Landed 2026-07-31** — most of it as 0050's phases 3b/2/3a/7a,
  then the per-list `page` (`#[wavedb::list(page = 25)]`), which exists because
  the two chain kinds have opposite write profiles: the built-in chain is
  rewritten whole at its growth end on every save and wants a small N, a list is
  rewritten in place and can hold the page a view renders. The counts are proven
  across a **crash and replay** too (`counts_survive_recovery`, over `PageStore`:
  a mid-batch tear during recovery is what makes the index and the segments
  disagree, and the test reads that disagreement off the public `list_len` vs
  the rows `listed` actually serves). What is left is two policy questions;
  [0054](0054-no-duplication-by-default.md) — which then **inverted the default**
  of all three (see above): duplication moved from something every collection pays
  to something a `#[wavedb::list]` asks for, and 0050's chain machinery is what
  both the pointer chains and the declared lists are built out of;
  [0053](0053-tenant-fair-cache-retention-PLANNED.md) — the policy that follows
  from that: which entries deserve to stay hot without one tenant monopolising the
  budget (navigational vs streaming, per-tenant accounting, no pinning ever). Held
  at *Planned* until a measured workload justifies it — the baseline of read,
  deposit, flush, and accept the miss is already correct;
  [0044](0044-page-cache-PLANNED-LOW.md) — a page-granular cache so the read
  that precedes a write also serves the settle's read-modify-write (the weaker
  answer to 0050's problem: it pays to tolerate a random access pattern rather
  than removing it — still worth having), **superseded 2026-08-01** by
  [0057](0057-page-arena-and-checkpoint-staging.md), which gives it a single
  block-aligned arena keyed by `BlockDescriptor` (copy-on-write descriptors make
  invalidation *disappear* rather than merely be cheap), lets a settle round
  assemble its window inside that arena so the pages it just wrote stay resident
  for the next round, and — the reason it opened — records why two neighbouring
  proposals are rejected: a crash-recovery "was this already written?" check
  (there is no fault to detect; a `Commit` retires only the rotated-out journal,
  so data in `data.bin` ahead of the boundary is never *only* there), and
  pre-building the checkpoint window between checkpoints (a page image depends
  on its bucket's full contents and on a zstd dictionary that is not final until
  the round closes);
  [0058](0058-per-type-actors-PLANNED.md) — the concurrency the two above ran
  into: `drain`/`settle`/`commit_journal` are synchronous `fn`s on a
  current-thread runtime, so a checkpoint blocks the request path by occupying
  the only thread — and that single-threadedness is *silently* supplying two
  invariants nothing documents (batch application being atomic, and
  `pending` empty ⇒ everything settled, which `evict_settled` trusts while
  `drain`'s `mem::take` has already emptied it mid-round). The answer is **one
  actor per type**, owning that type's whole family — the partition already
  exists in the storage layout (six slots per type; a batch never spans two user
  types), just not in the concurrency — with the journal and the writer staying
  single because one file means one fsync, and one fsync shared across many
  types' batches is group commit rather than a bottleneck. It needs a
  `Lane::Tree` first: `BPTREE_NODE_STORAGE` is one process-global slot for every
  type's B+tree nodes, and it is the only thing that makes the partition
  incomplete. **The engine becomes `Send` on native** (user decision
  2026-08-01, reversing `CLAUDE.md`'s non-`Send` stance): `Store`'s
  async-fn-in-trait desugars to `-> impl Future + MaybeSend`, a cfg'd bound in
  `wavedb-platform` keeps wasm paying nothing, and work-stealing then balances
  hot types automatically — which is what the pinned-thread alternative could
  never do, since an actor cannot migrate without its state. `Sync`, by
  contrast, mostly *disappears*, and that is the actor's doing rather than
  `Send`'s: actors own instead of sharing, so the locks that exist only to
  satisfy `Sync` on a `static` stop existing rather than stop contending;
  [0045](0045-vector-search-PLANNED.md) — nearest-neighbour search as a
  declared index kind (IVF over the existing `BpTree`, per-tenant centroids);
  [0056](0056-fuzzy-string-search-WIP.md) *(engine side landed 2026-08-01;
  no wire command, so a client reaches it through a `#[server]` fn — see
  `search_todos` in the todo-app)* — the same move for a different
  distance: `#[wavedb::fuzzy]` **on the field**, n-gram postings keyed
  `[gram][len][anchor]` in an ordinary `BpTree<SecKey>`, so one gram lookup is a
  `Bound::Prefix` scan and the length filter rides in bytes the scan already
  read. A count filter (`T - n*k` shared grams) bounds the candidates, then each
  survivor is verified at its anchor. Unlike a list it duplicates **no record
  bytes** — a posting is a pure function of the indexed field, so a save that
  left the field alone writes nothing at all; the price is `L + n - 1` scattered
  key writes per record.
- **Deferred (low priority):**
  [0036](0036-offline-write-queue-PLANNED-LOW.md) — W8 offline write queue
  (slice 1, Unique offline, *shipped*; the NonUnique/durable/conflict-surface
  remainder is deferred — the near-term focus is online + small offline),
  [0037](0037-multi-node-cluster-PLANNED-LOW.md) (multi-node cluster),
  [0038](0038-argon2-and-oauth-credentials-PLANNED-LOW.md) (Argon2/OAuth),
  [0039](0039-developer-experience-PLANNED-LOW.md) (M9 dev tooling),
  [0055](0055-sparse-index-merge-PLANNED-LOW.md) — the sparse index's merge,
  redistribute and root collapse: it grows and never shrinks, which is bounded
  (one entry per *segment*, so drained is still two or three levels) but
  permanent, and the sideways read a merge needs has to go through the
  `Overlay` rather than the `Store` — the reason this half of 0050 phase 3a
  was skipped while the rest landed.
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
| [0048](0048-chained-addressing-log.md) | The addressing log as a chain | Implemented |
| [0049](0049-elastic-pages-and-load-driven-splits.md) | Elastic pages and load-driven splits | Implemented |
| [0050](0050-clustered-record-chains.md) | Clustered record chains (B+trees become opt-in) | Implemented |
| [0051](0051-ordered-record-lists.md) | Declared lists: sorted chains + sparse index | Implemented |
| [0052](0052-segment-size-as-the-pagination-unit.md) | Segment size as the pagination unit | Implemented |
| [0053](0053-tenant-fair-cache-retention-PLANNED.md) | Tenant-fair cache retention | Planned |
| [0054](0054-no-duplication-by-default.md) | No duplication by default | Implemented |

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
