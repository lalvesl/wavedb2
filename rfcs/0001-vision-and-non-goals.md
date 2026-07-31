# RFC 0001 — Vision, motivation, and non-goals

- **Status:** Accepted
- **Crates:** all
- **Reference:** root `readme.md`

## Summary

WaveDB is a **database shipped as a Rust crate** — you compile it *into* your
application. The same schema and code run **client-side** (native and
browser/WASM) and **server-side** (storage nodes), so there is no separate
API/DTO/ORM layer to keep in sync: **the schema crate *is* the protocol.** Data
is **partitioned by tenant** from the first byte, history is **never
destroyed**, and a struct's identity is a **hash of its shape** so schema change
needs no migration step.

## Motivation — the layout problem in SQL

Conventional SQL organises storage **by type**, not by who owns the data or
what it relates to. Two consequences, both paid on every read:

1. **Every tenant's rows share one table.** `SELECT * FROM orders WHERE user_id
   = 42 AND amount > 100` — the partition key (`user_id`) rides in every query
   next to the real filter, and the engine walks pages holding thousands of
   unrelated tenants' rows to serve one.
2. **Unrelated child rows share one table.** One invoice's line items sit
   scattered among every other invoice's (and, compounded with #1, every other
   tenant's) — never colocated, so fetching a parent's children is a scatter of
   random page reads.

The root cause is laying out storage **by table/type instead of by access
pattern**. Application reads are almost always *"give me this one tenant's — and
often this one parent's — data,"* but the bytes that answer that are sprayed
across shared pages. WaveDB starts from that endpoint: the tenant is bound once
at connect (**structural, not a predicate**) and a collection's members are
reached through a per-collection index (`Pivot` → `BpTree`) so the bytes a read
needs sit together. The CPU saved from join processing is spent on compression
instead.

## The four defining properties

1. **Tenant-partitioned ownership.** Every record belongs to a tenant; the
   tenant is the unit of write-ownership (see [RFC 0007](0007-tenancy-and-data-ownership.md)).
2. **Horizontal distribution & redundancy** by tenant (multi-node; deferred —
   the model is designed for it, the serving path is not built).
3. **Timeline / history as a first-class citizen.** Saving never destroys the
   old bytes (see [RFC 0009](0009-anchors-succession-and-history.md)).
4. **Schema evolution by `STRUCT_HASH`.** A struct's identity is a hash of its
   shape, so changing it makes a new type; different builds coexist (see
   [RFC 0004](0004-struct-hash-and-schema-evolution.md)).

## The wave analogy (the name)

Frontend went static → client-rendered → server-rendered-dynamic; databases went
coupled-to-the-app → independent-with-ORMs → **back to application-centric, but
shipped as the library the full-stack app compiles in.** Each turn looks like
regression but carries the best of both forward — *"like ocean waves, going back
and forth but always advancing toward the shore."*

## Non-goals

- **Not OLAP.** Cross-tenant aggregation belongs in a separate analytics pipeline.
- **Not a general consensus system.** Consistency is tenant-scoped by design.
- **Not a SQL replacement.** No query DSL — reads are `get` / collection walk /
  `#[server]` functions ([RFC 0016](0016-server-functions.md)).
- **Not offline-first (yet).** The client cache stays strictly behind the node
  ([RFC 0024](0024-client-db-and-cache.md)); an offline write queue is future work.
