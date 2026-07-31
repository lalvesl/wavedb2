# RFC 0009 — Anchors, Succession, and history (DB-1)

- **Status:** Implemented (landed 2026-07-17 — the "DB-1" restructure)
- **Supersedes:** [RFC 0027 — doubly-linked modification chain](0027-doubly-linked-modification-chain-DEPRECATED.md)
- **Crates:** `wavedb-core`
- **Code:** `record.rs`, `core::mint`, `metadata.rs` (`crates/wavedb-core/src/`)

## Summary

Saving never destroys bytes. The **live** version of a record sits at a fixed
**anchor**; every superseded version is archived at a **derived slot** whose
address is a pure function of `(type, shape, authoring instant)`; versions chain
through `Metadata` by **instant, not address**. The chain records *who wrote
each version, when, and under which permission* — state review, not domain data
— and ends at the type's own `STRUCT_HASH` boundary.

## Motivation

History is a first-class property ([RFC 0001](0001-vision-and-non-goals.md)),
and the timeline doubles as the sync catch-up structure
([RFC 0022](0022-live-sync-navigation-catchup.md)). The earlier doubly-linked
design ([RFC 0027](0027-doubly-linked-modification-chain-DEPRECATED.md)) kept
`old`/`new` modification-id pointers, which meant a save had to **repoint an
existing archive** (an extra write) and the addresses were opaque handles. DB-1
removes both problems: if an archive's address is *computed* from its instant,
no archive is ever repointed and a link written once is correct forever.

## Design

### Anchors — where the live record lives

- **Unique:** `KEY = STRUCT_HASH`, `FLAG = 1`, `SALT = 0` — a directly
  computable address, so a Unique read is one lookup, no index walk.
- **NonUnique:** the immutable insert id (`KEY = CREATED_AT` of the insert,
  `FLAG = 0`). Identity is minted at `insert` and **never changes**; references
  point at it.

### Derived archive slots — where superseded versions go

When a save supersedes version *V*, *V* is archived at a slot derived from *V*'s
own authoring instant:

- `KEY` = that instant;
- `SALT = trunc(STRUCT_HASH)` — so two types can never collide in a flat
  keyspace (e.g. IndexedDB);
- `FLAG` = the anchor's bit **flipped** — an archive can never collide with an
  anchor, including a NonUnique V1 whose instant *is* the anchor key.

### The chain — `Succession`, instants not addresses

`Metadata` (full layout in [RFC 0010](0010-metadata-and-record-envelopes.md))
carries `previous: Option<u64>` and `succession: Succession`:

- **live** version: `Succession::CreatedAt(instant)` — its own authoring instant;
- **archive**: `Succession::Next(instant)` — the *successor's* instant.

Links are instants; addresses are computed from them. So:

- **no archive is ever repointed** (one write saved per save);
- a **forward walk that MISSes** a derived slot has reached the live anchor —
  the miss *is* the terminator.

### Concurrency — the `Expect` guard

Every save batch opens with `Write::Expect(id, bytes)`: a compare vs the
pre-batch state under the commit lock ([RFC 0008](0008-store-trait-and-atomic-batch.md)),
mismatch = typed `Error::Conflict`. Two concurrent saves of one anchor would
derive the *same* archive slot; the guard turns the loser into an honest
conflict instead of a lost update or overwritten history. Guards are validated
then **stripped** before journaling (never seen in replay).

### Monotone minting

Every instant goes through `key_nanos()` ([RFC 0005](0005-composite-ids-and-bit-budgets.md)).
Collections add a **floor**: `core::mint::mint_instant(floor)` =
`max(key_nanos(), floor+1, LAST+1)` against a process-wide watermark, where the
floor is the maxima of the recency/dead logs
([RFC 0011](0011-bptree-index-and-collections.md)). A rewound clock can never
write under a cursor a client already passed; Unique needs no floor (catch-up is
chain-forward, rewind-immune).

## Invariants

- `save` is an **upsert** — there is no `create`. A create that errors when the
  record exists is friction; `save` writes the anchor and archives the prior
  version.
- Bytes are never destroyed. Only `remove` writes the `dead` tree
  ([RFC 0011](0011-bptree-index-and-collections.md)).
- The chain is **state review, not domain data** — "member since" belongs in
  the record's own fields, not `Metadata`.

## Alternatives

- **Doubly-linked `old`/`new` pointers** — [RFC 0027](0027-doubly-linked-modification-chain-DEPRECATED.md),
  superseded (repoint-on-save, opaque addresses).
- **Journal commit-cursor as the history/sync log** —
  [RFC 0028](0028-journal-commit-cursor-sync-DEPRECATED.md), rejected the same
  day: rotated journals are deleted and `Batch` frames are physical not logical,
  so the disk record itself (anchors + derived slots + logs) becomes the sync
  log instead.
