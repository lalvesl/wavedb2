# WaveDB Application Platform — Roadmap

Goal: WaveDB as the foundation of a real full-stack application — **one schema
crate** shared by browser/native clients and by the serving nodes, "Next.js-style"
code sharing where the DB **is** the server and ships as a library.

```
my-app/
├── crates/
│   ├── app-schema/   # #[wavedb] structs + declare_objects! + evolution hooks
│   │                 #   compiled into EVERY binary below
│   ├── app-server/   # #[server] functions, validation, jobs (server-only)
│   ├── app-node/     # fn main() { wavedb_quick_node::run(config, REGISTRY, hooks) }
│   ├── app-client/   # native client binary / library
│   └── app-web/      # wasm32 build, typed API over IndexedDB + WebSocket
```

One `cargo build` workspace, several artifacts, zero DTO layer: the schema crate
is the protocol.

> Status: clean rebuild. Nothing below is implemented yet — this is the target
> sequence, written against the current design (see the crate READMEs).

---

## M1 — Foundations: schema crate compiles everywhere

The `app-schema` crate (`#[wavedb]` + `declare_objects!`) builds for native and
`wasm32`, producing `STRUCT_HASH`es, `Wire` impls, descriptors, the registry, and
the auto-generated `Pivot`/`BpTree` types. This is the keystone — every other
milestone consumes the registry.

**Exit:** the schema crate builds on both targets; the registry is queryable by
`STRUCT_HASH`; round-trip `Wire` encode/decode is property-tested.

## M2 — Storage engine

The block manager, per-`STRUCT_HASH` linear-hashed page directory, page format,
dictionaries, and the journal-first / `BTreeMap`-cache / background-settle
pipeline. Crash recovery via journal replay.

**Exit:** durable single-node `get`/`save` for Unique and `insert`/`delete` for
NonUnique through `Pivot`/`BpTree`; kill-during-write test recovers cleanly.

## M3 — Registry-aware node

A node that links the schema enforces shapes and serves records: header check →
decode → `validate` → `preprocess` before commit; Unique `get` and NonUnique
collection walks (`Pivot` → `BpTree`) served from storage.

**Exit:** clients' `get` and collection reads return real data from storage
through a registry-linked node; cross-tenant read without a grant is refused.

## M4 — Typed client API + server functions, end-to-end

`Db::connect`, typed CRUD, collection navigation through `PivotId`, delete → dead
`BpTree`, and **server functions** (`#[server]`: server-only body + client
binding, `Wire`-encoded args/return over `wavedb-net`, dispatched by `FN_HASH`) —
the replacement for a query DSL. The `first_try` / `fallback_not_found` hooks
bridge mixed-build clusters.

**Exit:** example apps run against a live node instead of in-process mocks; a
filtered read works through a `#[server]` function end to end.

## M5 — Browser target

`wavedb` + `wavedb-net` compile for `wasm32`; IndexedDB key→value store (no pages,
no journal); `gloo_net`/`fetch` transports behind the same async API. Measure the
registry's per-struct wasm cost.

**Exit:** a browser demo performs typed `save` + collection reads (and a
`#[server]` call) against a node over WebSocket, with IndexedDB caching reads.

## M6 — Local cache & `Db::open`

`LocalStore` trait (`get`/`put`/`scan`/`delete` over wire bytes): native
write-through file store, wasm IndexedDB. Reads hit local first, miss → fetch from
owner → back-fill; writes go to owner and write-through on ack. Read-your-writes
between the local store and notifications.

**Exit:** client survives node restart with warm local reads.

## M7 — Live sync

Owner node fans out mutations to subscribed sessions (WS push; HTTP piggyback +
idle ticks). Bloom-filter screen-sync. Client event API
(`Order::watch(&db)` over a collection / key).

**Exit:** client A saves; client B's watcher fires within one round-trip (WS) /
one poll tick (HTTP).

## M8 — Auth & permission enforcement

Stateless HMAC session tokens (signed with the cluster key, carry
`user`/`tenant`/expiry/purpose); node derives identity from the **token, never
the request body**. Login is a `#[server]` function minting the token from either
a local Argon2 credential object **or** an external OAuth/OIDC provider (same path,
same token). Unauthenticated tier (`user = U48::MAX`) restricted to login + public
reads. Permission checks on every read/write/delete (tenant-only / public /
tenant-list) — applied inside server-function bodies too, since they run on the
node. Full structure: [`wavedb-net`](../crates/wavedb-net/README.md#authentication).

**Exit:** cross-tenant access without a grant rejected at the node; a server
function rejecting a write surfaces as a typed client error.

## M9 — Developer experience

`cargo-generate` template (the workspace skeleton above, one struct per shape, a
`first_try`/`fallback_not_found` example, node + native + wasm binaries, a local
dev-cluster). "Building an
app on WaveDB" guide; schema-evolution cookbook (`first_try` /
`fallback_not_found` patterns).
Versioning policy for the platform crates.

---

## Deferred (explicitly out of scope for this rebuild)

- **Slow-node / cold history tier** (flush-down, archive reads).
- **Permission groups.**
- **`STRUCT_HASH`-grained write-ownership** — tenant-grained for now.
- **Offline-first reconciliation.**

## Sequencing

```
M1 (schema) ─► M2 (storage) ─► M3 (node) ─► M4 (typed E2E) ─► M7 (live sync)
                                   │                │
                                   │                └─► M8 (auth/permissions)
                                   └─► M5 (wasm) ─► M6 (local cache)
                                                          M9 (template) — last
```

## Risks / open questions

| Risk                                                              | Mitigation                                                                                                |
| ----------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Registry statics bloat the wasm binary as schemas grow            | M5 measures per-struct cost early; descriptors are `'static` data, dictionary-compressible by `wasm-opt`. |
| Server functions need stable identity across client/server builds | `FN_HASH` (name + arg types + return type) bound at compile time; a signature change is a new function, caught at the boundary. |
| Runtime abstraction (tokio vs wasm) leaks into public API         | Keep it internal to `wavedb`/`wavedb-net`; public API stays `async fn`.                                   |
| ID / block-descriptor bit budgets                                 | Resolved: `Id` = `KEY u64·TENANT u48·FLAG 1·SALT 15`; descriptor `u40·u20·u4` (pages + dictionary).       |
