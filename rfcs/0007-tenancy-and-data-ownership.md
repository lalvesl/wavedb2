# RFC 0007 — Tenancy and data ownership

- **Status:** Implemented (single-node; multi-node deferred)
- **Crates:** `wavedb-core`, `wavedb-quick-node`
- **Depends on:** [RFC 0001](0001-vision-and-non-goals.md), [RFC 0005](0005-composite-ids-and-bit-budgets.md)

## Summary

A **tenant** is the unit of data ownership and write-authority. Every record
belongs to exactly one tenant (the 48-bit `TENANT` field of its `Id`). Data of
different tenants **never mixes** in any structure — `Unique`, `NonUnique`,
`Pivot`, or `BpTree`. The tenant is bound **once, at connect/session open**, and
never appears in a read again.

## Motivation

This is the property that answers the SQL layout problem
([RFC 0001](0001-vision-and-non-goals.md)): if the partition key is
*structural* — baked into the id and the tree scope — rather than a *predicate*
restated in every query, then a tenant's bytes are never interleaved with
another's, and a read never walks past data it will throw away.

## Design

- **Tenant vs user.** A tenant is an organisation — a junction of many users
  (the B2B shape). For B2C the tenant number **equals** the user number: the
  organisation has exactly one user, itself.
- **Bound once.** `Db::connect(addr, user, tenant)` binds it; every typed op
  takes the tenant from the handle, never as an argument
  ([RFC 0024](0024-client-db-and-cache.md)). Node-side, the tenant is the
  token's claim, and a caller only ever executes in the tenant its token names
  ([RFC 0026](0026-auth-tokens.md)) — which *is* today's tenant-isolation
  enforcement.
- **Grouping is a storage optimisation, never sharing.** On disk, records are
  grouped only to make per-type zstd dictionaries and compression effective
  ([RFC 0018](0018-storage-engine.md)); grouping never crosses the tenant
  boundary and is never a permission mechanism.
- **Cross-tenant access is by grant.** A user of one tenant may read/write
  another tenant's data *only with permission* — the owner grants it via the
  record's `Metadata` ([RFC 0013](0013-permissions.md)). `db.as_tenant(t)` is
  the server-side seam for cross-tenant work (the register/bootstrap pattern).

## Write-ownership and distribution (deferred)

The tenant is designed to be the unit of **write-ownership across a cluster** —
more than one node serves and stores a tenant's data, and ownership of writes is
assigned by tenant (and, later, by `STRUCT_HASH`). The multi-node serving path
(ring ownership, gossip, replication, routing/failover) is **deferred** ([RFC 0037](0037-multi-node-cluster-PLANNED-LOW.md)); the
model is built for it, the mechanism is not. Single-node today: one open
`PageStore` per process ([RFC 0018](0018-storage-engine.md)).

## Alternatives

- **Tenant as a query predicate** (classic multi-tenant SQL): the exact thing
  this design exists to avoid — it reintroduces the interleaving and the
  restated partition key.
