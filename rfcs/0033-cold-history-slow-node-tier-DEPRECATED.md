# RFC 0033 — Cold/history slow-node tier — DEPRECATED

- **Status:** Deprecated — removed (single-tier history accepted)
- **Was:** a proposed second storage tier for aged history
- **Related:** [RFC 0009 — Anchors, Succession, and history](0009-anchors-succession-and-history.md),
  [RFC 0018 — The storage engine](0018-storage-engine.md)

## What it proposed

A separate **slow-node** / cold tier to hold aged history: since saves never
destroy bytes ([RFC 0009](0009-anchors-succession-and-history.md)), `data.bin`
grows without bound, so old archived versions would migrate to a cheaper,
slower store, keeping the hot `data.bin` small. A dedicated crate and cluster
monitors were sketched for it.

## Why it was removed

Premature for a database that does not exist yet
([RFC 0002 §8](0002-architectural-hard-rules.md), no-versioning policy). A second
tier adds a migration path, a routing decision (hot vs cold), and cross-tier
consistency — real complexity, to solve a growth problem no deployment has hit.
The honest choice for the rebuild phase is to **accept unbounded single-tier
growth** and defer compaction.

## What is done instead

History is a **single tier in `data.bin`** ([RFC 0018](0018-storage-engine.md));
unbounded growth is accepted for now. Pruning / compaction / archival is
explicitly deferred future work, to be designed against a real workload rather
than speculatively. The slow-node crate and cluster monitors are intentionally
absent.
