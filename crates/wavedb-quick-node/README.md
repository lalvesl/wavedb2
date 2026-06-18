# wavedb-quick-node

The **serving + storage node**. A node owns tenants via a consistent-hash ring,
takes routing ownership for connected users, validates and preprocesses writes,
stores data through [`wavedb-storage`](../wavedb-storage/README.md), and
replicates to peers. **Server and database are the same binary** — and the same
schema crate the clients compile.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module        | Responsibility                                                  |
| ------------- | --------------------------------------------------------------- |
| `server`      | The `Server` builder — bind, tenant, data_dir, registry, serve. |
| `serve`       | Request handling loop.                                          |
| `ring`        | Consistent-hash ring; derives tenant ownership.                 |
| `gossip`      | Membership: `Announce` / `Withdraw`, heartbeat.                 |
| `ownership`   | Tenant ownership scopes (one writer, n replicas).               |
| `replication` | Post-commit fan-out to the replica set.                         |
| `config`      | Node configuration.                                             |

```rust
Server::bind("0.0.0.0:7700")
    .tenant(42)
    .data_dir("./data")
    .registry(app_objects::REGISTRY)
    .serve()
    .await
```

---

## Write-ownership model

**Ownership is never configured — it is computed.** Every node derives who owns a
**tenant** from the consistent-hash ring: same membership view ⇒ same answer, so
agreement needs no handshake. Gossip moves the membership:

- **Join:** `Announce` → immediately owns its ring share (minimal reassignment).
- **Solo:** one node _is_ the ring — owns every tenant, zero setup.
- **Graceful leave:** drain → `Withdraw` → the ring drops it.
- **Crash:** heartbeat (default 1 s) evicts after 3 misses; ownership re-derives
  to survivors instantly. No records move — replicas already hold the data.

Ownership is **per tenant** today (the tenant is the write-ownership unit).
Finer-grained ownership keyed additionally by `STRUCT_HASH` is a planned
refinement of the same ring walk; the data model already leaves room for it. The
owner is the only writer: it validates, notifies replicas, and (later) forwards
history to the cold tier.

---

## Replication

Each tenant lives on **≥ 2 nodes** (`MIN_REPLICAS`): the ring owner plus the next
distinct nodes clockwise. After the owner's journal commit it pushes committed
bytes to replicas (`POST /replicate`, fire-and-forget with per-peer ack
watermarks) — the client's confirmation never waits on replicas. Durability to
the caller is the owner's journal; redundancy is asynchronous. Replicas store
canonical (validated + preprocessed) bytes verbatim and never accept client
writes. Rack-aware placement is a planned refinement of the same walk.

---

## Routing & failover

A client knows **two** nodes — owner and backup (both URLs returned at connect).
It writes to the owner; on timeout it switches to the backup and asks for the new
owner. A mutation that hits the wrong node is proxied forward or — usually cheaper
— triggers an ownership-transfer request. Tenant moves are a runtime operation,
not a deployment.

---

## Consistency

- **Within one tenant:** strong (single writer via routing ownership).
- **Across tenants:** eventual (Bloom-filter sync — see
  [`wavedb-net`](../wavedb-net/README.md#bloom-filter-screen-sync)).
- **On conflict:** most recent state wins via the live record; the loser becomes a
  branch in the history chain.

---

## Node-side enforcement

Every incoming write passes these gates **before the journal commit**:

1. **Header check** — the record's `STRUCT_HASH` must be declared in
   `declare_objects!`; unknown hashes refused.
2. **Decode check** — the payload must parse as the declared type via `Wire`.
3. **`validate`** — the same fn the client ran (catches bypassers / stale rules).
   This is the security boundary.
4. **`preprocess`** — the re-encoded result replaces the client's bytes.

Rejections travel back as a structured `NodeError {code, struct_hash, field,
message}` and the client maps it to the same typed `Error::Validation` its local
pre-send check raises. A node built without a registry keeps the legacy
schema-blind behaviour (opaque bytes). Hook declaration lives in
[`wavedb-macros`](../wavedb-macros/README.md#validation--preprocessing-hooks).

## Server-function dispatch

The registry also holds the `#[server]` functions. A `CallServerFn { fn_hash,
Wire args }` request is dispatched by `FN_HASH` to the function's server-only
body, which runs on the node with full DB access; the `Wire`-encoded return
travels back over the same transport. There is no query DSL — filtered/derived
reads are these functions. Permission checks apply inside the body. See
[`wavedb-macros`](../wavedb-macros/README.md#server-functions--server).

---

## Hardware & history tier

Good CPU/RAM, fast NVMe. Holds active records and the in-memory write cache. A
separate cold/history tier to which older versions flush down is **deferred —
not the moment**; the node keeps full history locally until that tier lands.
