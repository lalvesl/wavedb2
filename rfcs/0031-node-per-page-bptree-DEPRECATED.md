# RFC 0031 — One-node-per-page B+tree format — DEPRECATED

- **Status:** Deprecated — dropped (2026-07-07, user decision)
- **Was:** a proposed dedicated on-disk B+tree layout (S5)
- **Related:** [RFC 0011 — B+tree index, collections, and Pivots](0011-bptree-index-and-collections.md),
  [RFC 0018 — The storage engine](0018-storage-engine.md)

## What it proposed

A dedicated **32 KiB, one-tree-node-per-page** B+tree format — each B+tree node
gets its own page, sized for a large fan-out, so tree navigation is one page read
per level with plenty of room per node.

## Why it was dropped

The format optimises for a few large trees, but WaveDB's dominant case is the
**opposite**: trees are per tenant per collection, so a B2C deployment has
**millions of small trees** ([RFC 0011](0011-bptree-index-and-collections.md)). A
page per node would waste almost the entire page on every one of those tiny trees
— exactly the common case made expensive. The cost/benefit inverts against the
real workload.

## What is done instead

B+tree nodes live as ordinary `STRUCT_HASH`-headed values in the shared
per-`STRUCT_HASH` page directories ([RFC 0018](0018-storage-engine.md)), packed
with everything else — so a small tree costs a few slots, not a fleet of
near-empty 32 KiB pages. A dedicated large-tree format can be revisited if a
workload ever justifies it; the millions-of-small-trees case does not.
