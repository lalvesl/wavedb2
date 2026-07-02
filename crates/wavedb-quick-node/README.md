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
    .registry(app_schema::REGISTRY) // emitted by the schema crate's expose_server!
    .serve()
    .await
```

---

> **Status: single-node for now.** The current rebuild targets one node.
> Durability is the **journal** (a write is durable once journaled, before any
> replica). The multi-node machinery below — ring ownership, replication,
> routing/failover — is the **target design, deferred** until the single-node
> engine is solid. The async-replication durability window (owner crash before
> replicating) is therefore moot for now: with one node the journal is the whole
> guarantee.

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

Every incoming command frame (`struct_hash` + `command` + payload) passes these
gates **before the journal commit**, in order:

1. **Identity** — the request's identity is the **verified Access token**, carried
   inside the WaveDB request envelope (no HTTP `Authorization` header) and trusted
   only after its HMAC + expiry check. `user`/`tenant` come from that token, never
   from an unsigned field of the operation. The connected tenant is bound at
   session open (simple apps: `tenant = user`); over WebSocket (deferred) the token
   is sent once at the handshake. A frame targeting another tenant without a grant
   is refused (cross-tenant serving path deferred).
2. **Header check** — the frame's `STRUCT_HASH` must be listed in the schema
   crate's `expose_server!` declaration (a per-hash `match` arm); unlisted or
   unknown hashes are refused — an unexposed struct or excluded op fails here,
   indistinguishable from a type that never existed.
3. **Decode check** — the payload must parse as the declared type via `WaveWire`
   (a `Get`/`Remove` payload is an `Id`, not a full record).
4. **Permission** — the record's access rule is enforced here (by shape, below).
5. **`validate`** — the same fn the client ran (catches bypassers / stale rules).
   This is the security boundary.
6. **`preprocess`** — the re-encoded result replaces the client's bytes.

Once the gates pass, the registry routes the frame — **`match struct_hash`** to
the concrete type, then **`match command`** (`Get`/`Save` for Unique,
`Insert`/`Update`/`Remove` for NonUnique) to that type's compile-time engine fn,
which runs the authoritative `Pivot`/`BpTree` + page writes through
[`wavedb-storage`](../wavedb-storage/README.md). The command match lives inside
the matched arm, so the concrete type never escapes — no `dyn`, no `Object` enum.

Rejections travel back as a structured `NodeError {code, struct_hash, field,
message}` and the client maps it to the same typed `Error::Validation` its local
pre-send check raises. A node built without a registry keeps the legacy
schema-blind behaviour (opaque bytes). Hook declaration lives in
[`wavedb-macros`](../wavedb-macros/README.md#validation--preprocessing-hooks).

### Permission enforcement

Permission is checked at gate 4, by shape:

- **Unique** — the record's `Metadata.permission` (`None` = tenant-only).
- **NonUnique** — **per-record `Metadata.permission` is authoritative** (a record
  may diverge from its collection). The collection's `Pivot` carries a
  **default** permission, applied to a record at `insert` when it specifies none
  and checked for collection-scope ops where no single record is loaded yet
  (`Insert`, `All`). So the gate reads the record's own metadata for
  `Update`/`Remove`/`Get(id)`, and the `Pivot` default for the collection entry
  points. The per-record copy is what makes an `Update` **atomic** — the single
  journal entry validates and rewrites the record's permission without reading
  the `Pivot`. Changing the `Pivot` default does **not** rewrite existing records;
  it only seeds new inserts. Model: [`wavedb-core`](../wavedb-core/README.md#permissions).

## Server-function dispatch

The registry also holds the `#[server]` functions — in the **same `struct_hash`
space** as structs, carried by the **same `CommandFrame`** (`struct_hash` +
`command` + `payload`). There is no separate call frame: the node cannot — and
need not — tell a function call from an object op at the frame level. The single
`match struct_hash` is the discriminator; a function arm (its hash composed from
the argument/return objects' hashes — no separate `FN_HASH`) ignores `command`,
decodes `payload` as the args tuple, and runs the server-only body with full DB
access. The `WaveWire`-encoded return travels back over the same transport. A
collection-returning fn **streams** its items back as a sequence of frames (an
async iterator on the client) rather than buffering a whole `Vec`. There is no
query DSL — filtered/derived reads are these functions.

**Every server function requires a logged-in session; only `#[server(public)]`
(e.g. `login`, `refresh`) is reachable from the unauthenticated tier.** The
login/auth check runs **inside the function body**, not in the dispatch `match` —
the registry only routes `struct_hash → body`, so the generated dispatch stays
uniform (one arm per function, no per-function auth policy in the match). The
macro injects the auth guard into the body for non-public functions; permission
checks also apply inside the body. See
[`wavedb-macros`](../wavedb-macros/README.md#server-functions--server).

---

## Hardware & history tier

Good CPU/RAM, fast NVMe. Holds active records and the in-memory write cache. A
separate cold/history tier to which older versions flush down is **deferred —
not the moment**; the node keeps full history locally until that tier lands.
