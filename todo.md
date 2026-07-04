# TO DO

Clean reimplementation of WaveDB. The docs describe the **target** design;
everything that has landed is in [`todo_done.md`](todo_done.md). Workspace
members today: wire, wire-derive, core, macros, storage, schema-smoke.
Excluded (not built yet): wavedb, wavedb-net, wavedb-quick-node, wavedb-wasm,
bench, test-cluster, todo-app. Remaining work, bottom-up:

## M2 tail — storage engine optimizations (`wavedb-storage`)

Correctness is in (durable single-node `Store`, journal replay, B+tree with
merge/rebalance, secondary indexes, version chain, per-type `StructStorage`,
zstd dictionaries). What remains is performance shape, not behaviour:

- **dedicated 32 KiB one-node-per-page BpTree format** — nodes currently ride
  the generic `SlotPage` directory under the reserved page-kind `STRUCT_HASH`;
  target: one node per 8-block run, 18-byte entries (`key 8 B + LocalId 10 B`),
  ≈1 819 entries/page, height ≤3 for ≤6.03 B records (layout specced in
  `wavedb-storage` README);
- **background settle + rebalance** — settle is inline with `apply` today:
  move to a drain task that writes cached pages into `data.bin` at its own
  pace, evicts settled entries from the cache budget, and runs `split_next`
  off the hot path; then **journal checkpointing** (truncate replayed frames
  once settled — today the journal grows unbounded and replay is full-history;
  this is also the point where `DictState::warm` persistence becomes
  load-bearing);
- **per-value (strings/blobs) heap compression** — page-level zstd exists;
  per-value is future work.

## M3 — registry-aware node (`wavedb-net` + `wavedb-quick-node` → members)

The node consumes `Exposure::execute` (the seam the struct surface already
provides) and drives `PageStore` by typed command dispatch.

- **`wavedb-net` foundations**: `Request { auth, frame }` +
  `CommandFrame { struct_hash, command, payload }` as `WaveWire` structs — one
  uniform frame for object ops and (later) server-fn calls; **transport =
  dumb tunnel**: no HTTP headers/cookies/status semantics, the POST body is a
  self-contained request; **HTTP POST only** (token re-sent per request;
  WebSocket deferred to M7); FIFO queue per client; `mock` in-process
  transport for tests;
- **typed per-command settling** — `PageStore` today settles by reading the
  `STRUCT_HASH` off a value's first 8 bytes; the node layer knows record vs
  index-node vs Pivot per command and settles typed;
- **node builder** (`wavedb-quick-node`): `Server::bind(addr).data_dir(dir)
  .registry(REGISTRY).serve()` — attach the `expose_server!` output, open
  `PageStore` with the registry's `storage_entries()`, serve. **Single node
  only** — no ring, no gossip, no replication (deferred);
- **enforcement gates**, in order, before the engine:
  identity (tenant bound at session open; token verification stubbed until
  M8) → header (`Exposure::knows`) → decode (`decode_check`) → permission
  (record `Metadata.permission`; Pivot default for `Insert`/`All`) →
  `validate` → `preprocess`;
- **structured errors**: `NodeError { code, struct_hash, field, message }`
  inside the WaveDB reply envelope (not HTTP status), mapped client-side to
  the typed `Error`;
- **streaming reads** over the transport: `All`/`search` as a sequence of
  item frames (back-pressured), not a buffered `Vec` — or the frame protocol
  lands here and the client-side iterator lands with M4;
- **exit:** a client `get` and a collection read return real data from
  storage through a registry-linked node over HTTP POST; a command naming an
  unlisted hash is refused; kill-during-write + reopen recovers.

## M4 — typed client + server functions (`wavedb` + `#[server]` → members)

The developer surface — what `examples/todo-app` is written against.

- **`Db` handle**: `Db::connect(url, user, tenant)` (native first) — owns the
  local `Store` + the transport; `db.as_tenant(t)` for server-side
  cross-tenant work (register/bootstrap pattern); `Drop` releases the
  session;
- **client local `Store`**: native file key→value write-through cache (no
  pages, no journal); reads hit local first, miss → fetch from node →
  back-fill;
- **re-plumb typed ops over `&Db`**: today's `Collection` takes
  `&store, tenant` — macro emission gains the `UniqueObject` /
  `NonUniqueObject` route through `Db` (`T::get(&db)`, `record.save(&db)`,
  `T::collection(&db, pivot)`, `create_pivot(&db)` — tenant comes from the
  handle); each call = local write-through + command frame send (`save()`
  emits `Update` for NonUnique);
- **`#[server]` proc-macro**: server-only async body + client stub with the
  same signature; fn `STRUCT_HASH` = SeaHash over
  `fn_name + each ARG::STRUCT_HASH + RETURN::STRUCT_HASH` (builtins fold a
  fixed wire tag; all `const`); rides the **same `CommandFrame`** — function
  arm ignores `command`, decodes `payload` as the args tuple; functions join
  the `expose_server!` / `expose_client!` lists (one hash space);
- **streaming returns**: a collection-shaped return
  (`impl Stream<Item = Result<T>>`) ships item-at-a-time over the transport;
  the client stub re-exposes the async iterator;
- **auth guard placeholder**: `#[server]` = login-required by default,
  `#[server(public)]` opens the unauthenticated tier — the macro injects the
  guard into the **body** now (uniform `struct_hash → body` dispatch), even
  though real token verification lands M8;
- **core `Error` helpers** the app surface needs: `not_found`,
  `already_exists`, `unauthorized` (typed variants, not strings);
- **`examples/todo-app` compiles and runs** against a live node — the
  functions-only allowlist end-to-end: `register`/`login` (system tenant 0,
  username registry via secondary index, `as_tenant` bootstrap),
  `add_todo`/`all_todos`/`complete_todo`/`delete_todo` (profile→pivot path);
- **exit:** a filtered read works through a `#[server]` function end to end
  against a live node; todo-app runs its full client flow.

## M5 — browser target (`wavedb-wasm` → members)

- **IndexedDB `Store`**: key = 128-bit `Id` (big-endian), value = wire bytes;
  `apply` = one IDB readwrite transaction; no pages, no journal (IndexedDB
  already does block management + crash safety);
- `wavedb` + `wavedb-net` compile for `wasm32`: browser `fetch` POST
  transport, `wasm_bindgen_futures` runtime — same async API;
- the `Store`-generic `BpTree`/`Collection` already run over any backend —
  serverless mode (engine in-browser over IndexedDB) falls out;
- **measure the per-struct wasm cost** of the registry `match` (M1 risk
  item);
- **exit:** a browser demo performs typed `save` + a collection read + a
  `#[server]` call against a node, IndexedDB caching reads.

## M6 — local cache & `Db::open`

- `Db::open` family (native file path / wasm IndexedDB) with the local store
  as a real write-through cache: read-your-writes between local store and
  notifications;
- **exit:** client survives node restart with warm local reads.

## M7 — live sync (WebSocket lands here)

- WebSocket transport: token once at handshake, connection-bound identity;
  push notifications; HTTP piggyback + idle-tick fallback for POST clients;
- Bloom-filter screen-sync (client sends filter of on-screen `Id`s, node
  pushes deltas); client event API (`T::watch(&db)`);
- **exit:** client A saves; client B's watcher fires within one round-trip
  (WS) / one poll tick (HTTP).

## M8 — auth & permission enforcement

- **stateless HMAC access token** (short TTL; carries `user`/`tenant`/expiry/
  `purpose`; verified per request, no store) riding **inside the request
  envelope**, never an HTTP header;
- **refresh token** bound to a session record (`{ user, tenant, issued,
  revoked }`): rotate on use, replay = theft signal → revoke; revocation =
  one record write;
- `login` / `refresh` as `#[server(public)]` fns: local **Argon2** credential
  object (replaces todo-app's placeholder sha256) or external OAuth/OIDC —
  same path, same token pair;
- unauthenticated tier `user = U48::MAX`: login + `Public` reads only;
- **permission gate goes live** (gate 4): Unique = record
  `Metadata.permission`; NonUnique = two-level (record authoritative;
  `Pivot` default seeds inserts, gates `Insert`/`All`); checks apply inside
  server-fn bodies too;
- `Metadata.user` = real authorship from the verified token (today stamped
  `user = tenant`);
- **exit:** cross-tenant access without a grant rejected at the node; a
  revoked session's next `Refresh` fails and its access token dies within one
  TTL.

## M9 — developer experience

- `cargo-generate` template (schema/server/node/client/web workspace
  skeleton, one struct per shape, hook examples, dev-cluster);
- "Building an app on WaveDB" guide + schema-evolution cookbook
  (`first_try` / `fallback_not_found` patterns);
- versioning policy for the platform crates (version discipline starts at
  first release — `FORMAT_VERSION` unpinned from 1).

## Deferred (explicitly not the moment)

- **multi-node cluster** — ring ownership, gossip, replication,
  routing/failover (`wavedb-quick-node` docs hold the target design);
- **cold/history tier (slow-node)** — removed; history single-tier in
  `data.bin`, unbounded growth accepted; pruning/compaction/archive later;
- **permission groups**;
- **`STRUCT_HASH`-grained write-ownership** (tenant-only for now);
- **cross-tenant read _path_** (multi-node routing + grant enforcement) —
  model stays, serving path deferred;
- **offline-first reconciliation**;
- `update_call` exposure kind; per-user-session `SALT` masking.

## Resolved bit budgets

- **ID** = `KEY u64 + TENANT u48 + FLAG 1 + SALT 15 = 128`. No reserved bits.
- **LocalId** = `KEY u64 + FLAG 1 + SALT 15 = 80` (10 bytes). `Id` without
  `TENANT` for BpTree-internal pointers — tenant known from tree scope.
- **Block descriptor** = `start u40 + count u20 + occupation u4 = 64`
  (~4 PiB/file, ~4 GiB/page, 1/16th occupation). One format for pages **and**
  dictionary.

# DOING

- **M3 node — LANDED** (`wavedb-net` + `wavedb-quick-node` now members):
  `Request`/`Response`/`NodeError` wire envelopes, hand-rolled HTTP POST
  dumb tunnel, `NetClient`, and the `Server`/`Bound` builder driving
  `PageStore` through `Exposure::execute`. `expose_server!` also emits the new
  `StorageRegistry` impl, so `.registry(REGISTRY)` opens the engine. Proven
  end-to-end (`tests/node_http.rs`): Unique + NonUnique over the wire, uniform
  unknown-hash refusal, durable reopen. Gates 4–6 (permission/validate/
  preprocess) and typed per-command settling are the seams left for M8/later;
  streaming reads (`All`/search over the transport) land with the M4 client
  iterator.
- **M4 client core — LANDED** (`wavedb` now a member): the `Db` handle
  (`connect` / `as_tenant` / `tenant`), the typed CRUD surface, and
  `wavedb::Error` with the `not_found` / `already_exists` / `unauthorized`
  helpers. Unique `db.get::<T>()` / `db.save(&v)` and NonUnique
  `db.collection::<T>(pivot)` → `insert` / `get` / `save` / `remove` /
  `all`, all over HTTP POST into a live node (`tests/client_e2e.rs`). New
  core markers `UniqueStruct` + `PivotHandle` (macro-emitted) gate the two
  shapes. Collection walk lands as `Command::All` → buffered `Vec` (streaming
  frames deferred).
  - **Spelling note:** the client surface is `db.get::<T>()`, **not** the
    documented `T::get(&db)` — the macro already emits the `Store`-generic
    `T::get(store, tenant)` inherent methods, and inherent wins method
    resolution, so the two can't share a name yet. Unifying them means
    re-plumbing those inherent methods onto the `__WaveDbDb` generic.
- **M4 remaining — `#[server]` functions + streaming.** The big piece: a
  server-side execution context (a node-side `Db` running typed ops against
  the **local** `PageStore`, not the network), the fn `STRUCT_HASH`
  composition, the client stub, the in-body auth guard, and streamed returns.
  The clean target is one `Db<B: Backend>` (client backend = send a frame,
  server backend = run the core fn) so the same typed surface resolves on
  both sides. Until this lands, `examples/todo-app` (functions-only) cannot
  compile.
- **M2 tail** (`wavedb-storage`) stays open but blocks nothing: the dedicated
  **32 KiB one-node-per-page** BpTree format, **background** settle / rebalance
  + journal checkpointing, per-value heap compression.

_Workspace green: fmt + clippy (pedantic + nursery) clean, 25 test suites,
file-length gate passing. Members: wire, wire-derive, core, macros, storage,
net, quick-node, wavedb, schema-smoke._
