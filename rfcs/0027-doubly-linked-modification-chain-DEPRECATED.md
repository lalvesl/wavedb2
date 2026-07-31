# RFC 0027 — Doubly-linked modification chain — DEPRECATED

- **Status:** Deprecated — superseded by
  [RFC 0009 — Anchors, Succession, and history](0009-anchors-succession-and-history.md)
- **Was:** the pre-rebuild / early history model

## What it proposed

Each version's `Metadata` carried two pointers — `old_modification_id`
(backward) and `new_modification_id` (forward) — forming a doubly-linked list of
versions. A `save` wrote the new live version, wrote the superseded version to
history, and **repointed** the previously-latest archive's `new_modification_id`
to name the freshly-written version. `update` needed no `dead`-tree write because
the chain preserved the old version. (This is the model still described in older
prose in the root `readme.md`.)

## Why it was replaced

1. **A save had to repoint an existing archive** — an extra write on the hot
   path, and a mutation of already-written history (write-in-place on data meant
   to be immutable).
2. **Addresses were opaque handles**, so nothing about a version's *address*
   could be derived; the chain could only be walked by chasing pointers.
3. It gave the sync layer nothing navigable — catch-up still needed a separate
   log ([RFC 0028](0028-journal-commit-cursor-sync-DEPRECATED.md)).

## What replaced it

[RFC 0009](0009-anchors-succession-and-history.md) (DB-1): the live record sits
at a fixed **anchor**, superseded versions archive at **derived slots** computed
from their authoring instant, and links are **instants, not addresses**
(`Succession::{CreatedAt, Next}`). Consequences: no archive is ever repointed
(one write saved per save), a forward walk MISS reaches the live anchor, and the
recency/dead logs make the disk itself the sync structure
([RFC 0022](0022-live-sync-navigation-catchup.md)).
