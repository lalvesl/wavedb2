# RFC 0037 — Multi-node cluster

- **Status:** Planned (low priority) — deferred; model built, serving path not
- **Crates:** `wavedb-quick-node` (target design in its README),
  `wavedb-test-cluster` (excluded)
- **Depends on:** [RFC 0007](0007-tenancy-and-data-ownership.md),
  [RFC 0004](0004-struct-hash-and-schema-evolution.md),
  [RFC 0013](0013-permissions.md)

## Summary

Horizontal distribution and redundancy: more than one node serves and stores a
tenant's data, with **write-ownership assigned by tenant** (and, later, by
`STRUCT_HASH`). Ring ownership, gossip membership, replication, and
routing/failover. This is defining property #2 of WaveDB
([RFC 0001](0001-vision-and-non-goals.md)) — designed for from the first byte,
built last.

## Motivation

The data model is already shaped for it: the tenant is the unit of
write-ownership ([RFC 0007](0007-tenancy-and-data-ownership.md)), data never
mixes across tenants in any structure, and `STRUCT_HASH` lets different nodes run
different builds ([RFC 0004](0004-struct-hash-and-schema-evolution.md)). What is
missing is the *serving* path — the mechanism that routes a request to an owner,
replicates it, and fails over.

## Why low priority (deferred)

Single-node correctness comes first, and the research/rebuild phase deliberately
avoids format/version commitments ([RFC 0002 §8](0002-architectural-hard-rules.md))
that a replication protocol would freeze. Building a ring before the single-node
history/sync model settled would have meant redesigning it against DB-1
([RFC 0009](0009-anchors-succession-and-history.md)) — the sequencing is
intentional.

## Design (target — from the quick-node README)

- **Ring ownership** assigns each tenant to owner node(s).
- **Gossip** for membership and health.
- **Replication** for redundancy (more than one node holds a tenant's data).
- **Routing / failover** so a client reaches a current owner.
- The deferred **cross-tenant read path** rides here, and with it **gate 4**
  per-record grant enforcement ([RFC 0013](0013-permissions.md)) — today tenant
  isolation is the token binding itself, so grants have nothing to serve until
  cross-node routing exists.

## Blocked-on

The one-open-`PageStore`-per-process rule ([RFC 0018](0018-storage-engine.md)) is
a single-node artifact; a cluster node is still one process/one engine, but the
routing layer sits above it. `wavedb-test-cluster` stays in the workspace
`exclude` list until this lands.
