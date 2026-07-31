# RFC 0013 — Permissions

- **Status:** Partial (model built; per-record grant enforcement — gate 4 —
  deferred with the cross-tenant read path)
- **Crates:** `wavedb-core`, `wavedb-quick-node`
- **Code:** `permission.rs` (`PermissionRef`); `Metadata.permission`

## Summary

Access control is stored **inline in each record's `Metadata`**, scoped per
record. It is the mechanism by which a user of one tenant acts on another
tenant's data: the owner grants it.

| Setting | Who can access |
|---------|----------------|
| **Tenant-only** | Only the owning tenant's users (the common case). |
| **Public** | World-readable. |
| **Tenant list** | A specific list of other tenants. |
| **Group** | A shared permission group *(deferred — not implemented)*. |

## Motivation

Cross-tenant sharing must be *owner-driven* and *per record*, and an atomic
`Update` must be able to validate permission **without reading the Pivot** — so
the authoritative value lives on the record itself.

## Design

- **Per-record authoritative value.** Each record's `Metadata.permission`
  ([RFC 0010](0010-metadata-and-record-envelopes.md)) is the source of truth for
  that record, so `Update` gates on data it already has in the batch.
- **Collection two-level default.** A collection's `Pivot` holds a **default**
  that seeds new inserts and gates collection-scope ops (`Insert`/`All`); a
  record may override its collection. The per-record copy is what removes the
  Pivot read from `Update`.
- **Enforcement order.** Permission is **gate 4** of the node's enforcement
  chain ([RFC 0023](0023-quick-node-and-gates.md)), after identity/header/decode.

## What is built vs deferred

- **Built:** the model — `PermissionRef` on `Metadata` and the Pivot default,
  set from the verified caller.
- **Deferred (gate 4 enforcement):** per-record grant *checks* ride with the
  cross-tenant read *path*, which is itself deferred (it needs multi-node
  routing, [RFC 0007](0007-tenancy-and-data-ownership.md)). Today tenant
  isolation is the **token binding itself** — a caller only ever executes in the
  tenant its token names ([RFC 0026](0026-auth-tokens.md)) — so grants have
  nothing to serve yet.
- **Deferred:** permission **groups**, and `STRUCT_HASH`-grained
  write-ownership (tenant-only for now).
