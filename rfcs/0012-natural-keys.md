# RFC 0012 — Natural keys (`#[wavedb::key]`)

- **Status:** Implemented (landed 2026-07-19 — the DB-1 natural-key phase)
- **Crates:** `wavedb-core`, `wavedb-macros`
- **Code:** `collection_keyed.rs`, `core::natural_key_hash`, `mint::keyed_id`

## Summary

A NonUnique struct can declare the fields that **are** its identity with
`#[wavedb::key(f1, …)]`. The collection anchor `KEY` becomes a seahash over
those fields' wire bytes, so `insert` **is** the upsert at that content address.
Unique + `#[wavedb::key]` is a compile error; at least one field, at most one
declaration.

## Motivation

Without a natural key, NonUnique identity is a minted instant — fine for logs,
wrong for "one settings row per name" or "one membership per (user, group)".
The user asked for content-addressed identity — *"apenas use a seahash"* — so a
second `insert` of the same logical thing updates it rather than duplicating.

## Design

- **Anchor = `natural_key_hash`** over the key fields' wire bytes (one
  per-build derivation; `seahash` is a core dep). The declaration folds into
  `STRUCT_HASH` as a synthetic `#key` entry ([RFC 0004](0004-struct-hash-and-schema-evolution.md))
  — changing the key is a schema change.
- **`insert` = upsert at the content anchor** (`collection_keyed.rs`):
  - **vacant** → guarded first version (the `Expect(None)` path of
    `plan_chained_save`) + full indexing;
  - **living** → an ordinary chained save;
  - **dead** → **revival**: chains onto the whole prior history and re-enters
    current + recency + secondaries. The dead log keeps the removal as a
    historical event, so catch-up merges both tails by instant and replays
    `Removed` then `Saved` in order ([RFC 0022](0022-live-sync-navigation-catchup.md)).
- **`save`/`update` addressing a foreign anchor refuses** typed
  (`Error::KeyMismatch`) — renaming is an explicit `remove` + `insert`.
- **The keyed first version's `Metadata` instant is minted**, because `id.key()`
  is a hash, not an instant — the "key IS the authoring instant" rule of a
  time-keyed insert does not apply.
- **Keyed walks come out in hash order**, not insertion order; modification
  order lives in the recency log ([RFC 0011](0011-bptree-index-and-collections.md)).

## Consequences for mirrors

`adopt`/`adopt_with` learned the revival branch (bytes-at-anchor but not living
→ chain, don't overwrite), so a mirror archives its dead copy at the node's own
derived slot byte-identically ([RFC 0024](0024-client-db-and-cache.md)).

## Alternatives

- **A separate uniqueness index** rather than a content anchor: rejected — it
  would need its own tree write and a two-phase lookup on every insert, where
  the content anchor makes the upsert a single addressed write.
