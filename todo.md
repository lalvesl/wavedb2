# TO DO

Clean reimplementation of WaveDB. The docs describe the **target** design;
everything that has landed is in [`todo_done.md`](todo_done.md). Workspace
members today: wire, wire-derive, platform, core, macros, storage, net,
quick-node, wavedb, wasm, schema-smoke, contact-book, registry-size,
todo-app (schema/server/client). Excluded (not built yet): bench,
test-cluster. Remaining work, bottom-up (the task log is
in [PLAN — M4 completion](#plan--m4-completion) at the end; all tasks
T1–T7 landed):

## M2 tail — storage engine optimizations (`wavedb-storage`)

Correctness is in (durable single-node `Store`, journal replay, B+tree with
merge/rebalance, secondary indexes, version chain, per-type `StructStorage`,
zstd dictionaries). The tail is now largely landed (task log in
[PLAN — M2 tail](#plan--m2-tail-storage-engine)):

- **background settle + journal-rooted recovery — LANDED (S1, S4,
  J1–J5, 2026-07-07)**: page-backed read-through (cache is a cache),
  deferred settle behind a `pending` queue with unsettled-remove
  tombstones, and the user-directed **journal-rooted commit**: timestamped
  `journal_<ts>.log` rotation (no write lock), directory chains as CoW
  blocks in `data.bin`, ONE atomic `Commit` frame (roots of all types) in
  the new journal retiring the old one, superblock write-once again. The
  journal no longer grows unbounded; recovery roots in the newest valid
  `Commit`. (The interim S2/S3 superblock-pointer checkpoint was
  superseded the same day.) Node maintenance task: drain → threshold
  commit → cache eviction to budget;
- **dedicated 32 KiB one-node-per-page BpTree format — DROPPED
  (2026-07-07)**: trees are per tenant; B2C = millions of small trees, so
  a page per node wastes exactly the dominant case (see S5 in the PLAN);
- **per-value (strings/blobs) heap compression** — page-level zstd exists;
  per-value is future work, measure first (S6).

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

- [x] **platform seam — LANDED (2026-07-10)**: new bottom crate
  `wavedb-platform` (below core) owning the three per-target facts behind
  one API compiled two ways — `time` (`SystemTime` / `Date.now()`; on
  wasm32-unknown-unknown `SystemTime::now()` *panics*, so core's id minting
  and net's token clock route through it), `rand` (`RandomState` keys /
  `window.crypto.getRandomValues`; quick-node's default secret draws from
  it), and `http` (the tunnel's **client half**: hand-rolled TcpStream POST
  / `fetch` + `Request` with the response streamed through a
  `ReadableStreamDefaultReader`). `wavedb-net::frames::FrameReader` now
  reassembles `[len u32][bytes]` frames over the platform `Body` on both
  targets; the server half stays native in `wavedb-net::http`;
- [x] `wavedb` + `wavedb-net` (+ core, platform) compile and clippy clean
  for `wasm32-unknown-unknown`; `wavedb-wasm` is a **workspace member**
  shipping a raw `probe::call_fn_raw` export (anchors the client stack —
  size tracker reads 501 KB raw / 124 KB gzip pre-bindgen, 278 KB smaller
  than the last measurement);
- **no tokio inside wasm** (user constraint, 2026-07-07) — holds: tokio
  stays behind `cfg(not(target_arch = "wasm32"))` (platform + net gate it;
  client/schema carry it as dev-deps only); the wasm build runs on
  `wasm_bindgen_futures`. The wasm side has **no journal and no
  `data.bin`** — IndexedDB `Id → Vec<u8>` will be the whole store (the
  `Store` trait absorbs the difference);
- [x] **IndexedDB `Store` — LANDED (2026-07-10)**: `wavedb_wasm::IdbStore`
  — one `kv` object store, key = 128-bit `Id` (16 B big-endian, bytewise
  key order == numeric order), value = wire bytes; `apply` = ONE IDB
  readwrite transaction (complete = durable, error/abort = rolled back
  whole — the atomic-batch contract for free); no pages, no journal.
  `idb.rs` bridges the event-driven requests to futures (oneshot per op,
  closures dropped after the await — no per-op leak); faults convert to
  `core::Error::Backend` at the module edge. **Proven in real headless
  Chrome** (`tests/idb_store.rs`, run via the nixpkgs-chromedriver
  incantation in CLAUDE.md): raw batch roundtrip + reopen durability, and
  the typed serverless flow — `LocalHandle` + Unique `AboutUser` +
  NonUnique `Note` collection with BpTree + `by_pinned` secondary, all
  over IndexedDB;
- [x] the `Store`-generic `BpTree`/`Collection` run over the IDB backend —
  serverless mode (engine in-browser over IndexedDB) proven by the typed
  browser test above;
- [x] **registry-`match` per-struct wasm cost — MEASURED (2026-07-10)**,
  the M1 risk item, answered: **~23 B raw / 18 B gzip per exposed struct**
  once its decode shape exists in the binary (the pure `match`-arm cost),
  **~204 B raw / 44 B gzip** for a struct bringing a novel `WaveWire`
  decode (code a heterogeneous schema needs anyway). Measured by
  `examples/registry-size` (64 structs defined, exposure width
  feature-selected 1/16/64, runtime-hash probe defeats DCE) via
  `scripts/registry_size.sh`; numbers + method in that crate's README.
  Verdict: no sum type, no descriptor table — the registry scales;
- [x] **exit — LANDED (2026-07-10)**: the typed browser demo against a
  live node (`crates/wavedb-wasm/tests/live_node.rs`, run via
  `scripts/browser_demo.sh`: node = `cargo run -p contact-book --example
  node`, address baked in as `WAVEDB_DEMO_NODE`): a `#[server]` call
  (`open_book`/`contacts_in`), a typed Unique `save`, collection
  insert/update/remove + the streamed `all` walk — all over browser
  `fetch` — and IndexedDB caching reads (Unique read-through/back-fill +
  a locally re-walked collection over `IdbStore`). Unblocked by the CORS
  seam: the wasm `post` sends **no** `content-type` (bytes-only POST = a
  CORS simple request, no preflight for the POST-only server) and the
  node's `200`/`400` heads carry `access-control-allow-origin: *` (not a
  boundary — identity is the in-body token, never ambient credentials).

## M6 — local cache & `Db::open` — LANDED (2026-07-10)

- [x] `Db::open` family (native file path / wasm IndexedDB) with the local
  store as a real write-through cache — node-first (revalidation = every
  successful read), mirrors best-effort under node-minted ids
  (`Collection::adopt`), fallback only on transport faults the cache can
  answer, offline writes refuse (no queue until M7). Details in the DOING
  entry and `crates/wavedb/README.md`'s status block;
- [x] **exit held:** client survives node restart with warm local reads
  (`examples/contact-book/tests/local_cache_e2e.rs` — two processes, the
  node re-executed as a child of the test binary).

## M7 — live sync (WebSocket lands here)

Task log in [PLAN — M7 live sync](#plan--m7-live-sync) at the end.

- WebSocket transport: token once at handshake, connection-bound identity;
  push notifications; HTTP piggyback + idle-tick fallback for POST clients;
- screen-sync as **declared subscriptions + journal cursor** (client event
  API `T::watch(&db)` over Unique anchors / collection pivots; reconnect =
  "since journal sequence N" streaming exactly the tenant's deltas). The
  earlier Bloom-filter idea is **rejected** (2026-07-10): answering a filter
  on reconnect would force the node to test its whole dataset against it,
  and exact pivot/anchor subscriptions already give live filtering without
  false positives;
- offline write queue replaying through the same cursor path (M6 refuses
  offline writes on purpose — the cache never diverges);
- **exit:** client A saves; client B's watcher fires within one round-trip
  (WS) / one poll tick (HTTP). **The WS half holds (W1–W5 landed
  2026-07-13)** — `Db::watch_unique`/`watch_collection` push typed events
  and keep the M6 cache warm; remaining: W6 (reconnect cursor), W7 (the
  HTTP poll-tick half of the exit), W8 (offline queue).

## M8 — auth & permission enforcement — LANDED (2026-07-10)

What shipped (details in `todo_done.md`):

- [x] **stateless HMAC access token** (`wavedb-net::auth`): 15-min TTL,
  claims `{ user, tenant, expires_at, purpose, session, nonce }` +
  HMAC-SHA256, verified per request by the node's gate 1; rides inside
  `Request.auth` (`Auth::Anonymous { tenant } | Auth::Token(bytes)`), never
  an HTTP header;
- [x] **refresh token** bound to a `wavedb::auth::AuthSession` record
  (stored **hashed**): rotate on use, replay = theft signal → session
  revoked on the spot; revocation = one record write (`issue_pair` /
  `refresh_pair` / `revoke` over any `DbHandle`);
- [x] `login` / `refresh` / `logout` as `#[server(public)]` fns in todo-app,
  returning `wavedb::TokenPair`; the guard is macro-injected — a plain
  `#[server]` fn refuses `user == U48::MAX` before decoding;
- [x] unauthenticated tier `user = U48::MAX`: public fns only; every struct
  command refuses it uniformly (`Unauthorized`) in the generated steps;
- [x] **verified identity threads the whole stack**: `Caller { user, tenant }`
  through `Exposure::execute` → generated `__wavedb_*` steps →
  `ServerDb::for_caller`; `Metadata.user` = the token's user
  (`Collection::stamped_by`, `save_unique_as`);
- [x] node secret: `Server::secret([u8; 32])` or a random one per boot,
  published process-wide (`wavedb_net::auth::node_secret`) for the minting
  helpers — one node per process, like the engine slots;
- [x] **exit held** (`examples/todo-app` e2e): a claimed tenant cannot
  override the token's; anonymous non-public call refused; replayed refresh
  revokes the whole session; logout kills the next refresh; expired /
  forged / wrong-purpose tokens refused (dispatch unit tests).

Deliberately left as later seams:

- [ ] **Argon2** credential object (todo-app still hashes sha256) and the
  OAuth/OIDC path;
- [ ] **record-level permission grants (gate 4)**: `Metadata.permission`
  checks ride with the deferred cross-tenant read path — today tenant
  isolation is the token binding itself (a caller only ever executes in the
  tenant its token names), so grants have nothing to serve yet;
- [ ] gates 5–6 (`validate` / `preprocess`) — unchanged, the hook seam.

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
- **`#[server]` functions — LANDED.** A function declared once runs on the
  node against the local store and is called from the client over the wire.
  The macro emits a fn-type (identity + `__wavedb_dispatch`), the body retyped
  onto a node-side `ServerDb`, and a client stub; `expose_server!` gains
  `fn`-marked entries dispatched through the same registry. Proven E2E
  (`tests/server_fn_e2e.rs`).
- **M4 COMPLETE (2026-07-06)** — the exit criterion holds: `examples/todo-app`
  is a workspace member and runs its full flow against a live node (test +
  real binaries). What landed, one line each (details per task in
  [PLAN — M4 completion](#plan--m4-completion)): the **`DbHandle` seam**
  (core trait + `LocalHandle`, T1); the **macro re-plumb** to the unified
  `T::get(&db)` / `T::collection(pivot)` + `CollectionHandle` spelling (T2);
  **`Db` + `ServerDb` implementing the trait** and the interim
  `db.get::<T>()` surfaces deleted (T3); the **`store`-only exposure entry**
  for storage-only types (T4); **todo-app end-to-end** (T5); **streaming
  reads + stream-returning `#[server]` fns over framed wire** (T6); the
  **composed function identity** (`fn_identity::compose`, T7). The PLAN is
  fully landed — details in `todo_done.md`.
- **M2 tail** (`wavedb-storage`) stays open but blocks nothing: the dedicated
  **32 KiB one-node-per-page** BpTree format, **background** settle / rebalance
  + journal checkpointing, per-value heap compression.

- **M5 platform seam — LANDED (2026-07-10)**: `wavedb-platform` (new bottom
  crate) owns time/entropy/HTTP-client-half per target; core id minting, the
  token clock, quick-node's default secret, and `NetClient` all route through
  it. The whole client stack compiles + clippy-cleans for
  `wasm32-unknown-unknown`; `wavedb-wasm` is a member with a raw
  `probe::call_fn_raw` export (fetch → node → framed reply).
- **M5 IndexedDB `Store` — LANDED (2026-07-10)**: `wavedb_wasm::IdbStore`,
  flat `Id → wire bytes`, one readwrite transaction per `apply`. Proven in
  real headless Chrome: raw roundtrip + reopen durability + the typed
  serverless flow (Unique, collection, BpTree secondary index) over
  IndexedDB.
- **M5 COMPLETE (2026-07-10)** — both remaining items closed the same day:
  - **exit**: the typed browser demo runs against a live node in headless
    Chrome (`tests/live_node.rs` + `scripts/browser_demo.sh`, contact-book
    registry): `#[server]` calls, typed Unique save, streamed collection
    walk over `fetch`, IndexedDB caching reads. CORS unblocked as a
    transport-only change (no client `content-type` ⇒ no preflight;
    `access-control-allow-origin: *` on the node's heads);
  - **registry-`match` cost measured** (`examples/registry-size` +
    `scripts/registry_size.sh`): ~23 B raw / 18 B gzip per exposed struct
    (arm only), ~204 B raw / 44 B gzip with a novel decode shape — the M1
    risk is retired.

- **M6 COMPLETE (2026-07-10)** — `Db::open`: the client cache is WaveDB
  caching WaveDB, cfg-switched like the platform seam (native = the
  journal + `data.bin` `PageStore`; wasm = `IdbStore`, which moved from
  `wavedb-wasm` into `wavedb::cache` — the wasm crate re-exports it).
  What landed:
  - `Db::open(CLIENT_REGISTRY, addr, user, tenant, app)` both targets
    (`app` → auto-created `$XDG_CACHE_HOME|~/.cache/<app>` or
    `%LOCALAPPDATA%/<app>`, IndexedDB database named exactly `<app>`), native
    `open_at(…, dir)`,
    and `db.local()` (the cache's direct `LocalHandle` surface);
  - **semantics: node-first** (chosen over local-first + revalidation —
    every successful read IS the revalidation; Bloom screen-sync discarded
    for now): acknowledged ops mirror best-effort; fallback only on
    `Error::Transport` and only when the cache holds the answer (absence
    propagates the fault; `NodeError` refusals never fall back); offline
    writes refuse — the cache is strictly behind the node, no merge needed;
  - **core adopt seam** (`collection_adopt.rs`): `adopt_pivot` /
    `Collection::adopt` write **node-minted** identities into a local store
    (insert-at-id / save / skip-unchanged — read mirroring can't grow the
    store), shared with future M7 sync. `All` frames now carry `(Id, T)`
    so walks mirror under authoritative ids (same order, same ids warm);
  - `expose_client!` now also emits the native `StorageRegistry` (the
    engine slots `Db::open` registers); one open engine per process —
    a `Db::open` client and a node can't share one (child-process node);
  - `get_record` deliberately does **not** back-fill (it also resolves
    removed records; adopting one would resurrect it into the living walk);
  - proven: `local_cache_e2e.rs` (M6 exit: warm unique/by-id/ordered-walk
    reads through a node kill, honest refusals, restart + journal replay),
    `local_cache_cold.rs` (cold cache propagates the fault, `db.local()`
    warms, refused writes don't touch the cache), browser suite +
    `browser_demo.sh` re-run green with the moved `IdbStore` and new
    `All` frames.

- **Schema side features — LANDED (2026-07-13, user-directed)**: the
  server-code no-leak guarantee, stronger than LTO/DCE. A crate expanding
  `#[server]` / `expose_server!` / `expose_client!` declares cargo features
  named exactly `server-side` / `client-side`; the macros gate emission on
  them — body + `__wavedb_dispatch` + the whole `expose_server!` output
  under `server-side`, client stubs + `expose_client!` under `client-side`,
  the fn-type/`STRUCT_HASH` and all `#[wavedb]` struct machinery under both
  (the schema IS the protocol). The client/schema/server crate split stays
  (each side may carry unrelated code — jobs, UI); deployed binaries pull
  the schema `default-features = false` + their side, so the other side is
  never *compiled* in; defaults keep both on for the schema's own tests.
  Hand-written server-only helpers carry the cfg themselves (todo-app,
  schema-smoke show the pattern); `expose_server!` `compile_error!`s any
  wasm32 + `server-side` build; `wavedb-wasm` pulls its schema dev-deps
  client-side only. Proven: debug `todo-app-server` carries the body
  strings, `todo-app-client` carries none; contract in
  `docs/development_standards.md` + the macros README.
- **M7 W1–W4 LANDED (2026-07-11)** — the WebSocket transport + node push,
  a complete tested vertical slice (task log in
  [PLAN — M7 live sync](#plan--m7-live-sync)). Hand-rolled RFC 6455 in the
  same dumb-tunnel stance as the HTTP POST tunnel (`sha1` the one new dep;
  the Phase-11 `tokio-tungstenite`/`gloo-net`/`axum` stay unused). What
  landed: `wavedb_platform::ws` (codec + native handshake + browser
  `WebSocket` bridge, one surface two targets); `wavedb_net::ws` envelopes
  + the `http` head parser routing `GET`+`Upgrade` alongside POST; the core
  `Store::note_mutation` seam (+ `core::notify::Mutation`, + blanket
  `impl Store for Rc<S>`); and the node's `SubTable`/`NotifyStore` +
  `serve_ws` session loop (identity bound once at `Hello`, exact-topic push,
  FIFO `Call`s, `dispatch::execute` shared with HTTP). Proven by
  `node_ws.rs` (two connections, real frames, subscription events + an
  `Unsubscribe` isolation check).

- **M7 W5 LANDED (2026-07-13)** — client watch + cache sync; **the M7
  exit's WS half holds**: client A saves, client B's watcher fires within
  one round-trip, typed and in order (`live_watch_e2e.rs`, two processes).
  What landed: `wavedb_net::WsSession` (both targets: `Hello`→`HelloOk`,
  acked `subscribe`/`unsubscribe`, `next_event`; buffers events racing an
  ack); the protocol gained `ServerMsg::TopicOk` — subscription mutations
  are acked FIFO, so a returned watch **cannot miss** a later mutation
  (the W4 test's barrier-call pattern retired), and an anonymous
  `Subscribe` now closes the connection (loud refusal, nothing silent);
  `Db::watch_unique::<T>()` / `Db::watch_collection::<T>(pivot)` →
  `UniqueWatch`/`CollectionWatch` yielding `WatchEvent::{Saved(Id, T),
  Removed(Id)}`, each event **mirrored into the M6 cache before it is
  yielded** (`mirror_unique`/`mirror_record`/`mirror_remove`) — proven by
  a post-kill warm walk of a collection the watcher never read online; a
  token-less handle refuses `watch_*` typed (`Unauthorized`) instead of a
  dead socket. One WS connection per watch (multiplexing = later
  refinement); typed `T::watch(&db)` sugar joins the `T::get(&db)`
  unification note (watch is `Db`-only — no `DbHandle` seam for it yet).
  Next: W6 (journal-cursor catch-up), W7 (HTTP piggyback), W8 (offline
  write queue).

_Workspace green (both targets): fmt + clippy (pedantic + nursery) clean,
tests green, file-length gate passing. Members: wire, wire-derive, platform,
core, macros, storage, net, quick-node, wavedb, wasm, schema-smoke,
contact-book, registry-size, todo-app (schema/server/client). Still
excluded: bench, test-cluster._

# PLAN — M4 completion

The ordered tasks to the M4 exit (**`examples/todo-app` compiles as a
workspace member and runs its full flow against a live node**), grounded in
the code as of 2026-07-06. Dependency chain: T1 → T2 → T3 → T5, with T4
slotting in anywhere after T2; T6/T7 were post-exit refinements. Each task
lands green (fmt + clippy + tests + file gate) and moves here to
`todo_done.md` prose when done.

## T1 — core `DbHandle` seam — **DONE (2026-07-06)**

The one trait all three execution contexts implement, so generated methods
can say `T::get(&db)` regardless of what `db` is.

- [x] New `wavedb-core/src/handle.rs`: trait `DbHandle: Sized` with
      `type Error: From<core::Error>` (the client's error is richer than
      core's — an associated error keeps the node/transport variants without
      polluting `core::Error`), `fn tenant(&self) -> U48`,
      `fn as_tenant(&self, U48) -> Self` (the `register` bootstrap seam), and
      the op set: `get_unique<T: UniqueStruct>` / `save_unique` /
      `unique_history`, `create_pivot<T: NonUniqueStruct>`, and the
      record ops `insert` / `get_record` / `update` / `remove` / `all` /
      `search_by` (pivot passed as `LocalId`).
- [x] Walk-shaped ops (`unique_history`, `all`, `search_by`) return
      `impl Stream` **in the trait signature** even though the client impl
      buffers today (wraps its `Vec` in `stream::iter`) — T6 then changes the
      client's internals, not the surface. They carry `T: 'static` (free:
      `WaveWire` values are always owned) so the yielded items aren't tied to
      the handle borrow.
- [x] `LocalHandle<'a, S: Store>` in the same module: `{ store, tenant }`,
      `Error = core::Error`, pure delegation to `collection` / `record` fns.
      This is what core/storage/schema-smoke tests drive.
- [x] Unit tests: `LocalHandle` behaves identically to the direct core calls
      (insert/get/save/remove/all + unique round-trip + history).
- [x] Fallout fix: `Collection`'s read methods (`history` / `search` /
      `search_by` / `all`) now take `self` by value (the handle is `Copy`) —
      under edition-2024 RPIT capture rules a borrowed receiver tied the
      returned stream to a temporary at `T::collection(..).all(store)` call
      shapes.

## T2 — macro re-plumb onto `DbHandle` — **DONE (2026-07-06)**

Retire the store-based inherent methods; same names, handle-based
signatures. This is the breaking rename — one commit, all call sites.

- [x] `wavedb_attr.rs` `unique_ops`: `T::get<D: DbHandle>(db: &D) ->
      Result<Option<Self>, D::Error>`, `value.save(db)`, `T::history(db)`.
- [x] `generated.rs`: `T::collection(pivot: {N}PivotId) ->
      CollectionHandle<Self>` (still `const`; **no `db` arg** — the handle
      is pivot-only and a context parameter with zero semantics was API
      debt, so the todo-app spelling adjusts by one argument) and
      `T::create_pivot<D>(db: &D)`. New core `CollectionHandle<T>` (own file
      `collection_handle.rs`, budget): carries `pivot: LocalId` only;
      methods take `&D` per call — `col.insert(db, v)`, `col.get(db, id)`,
      `col.save(db, id, v)`, `col.remove(db, id)`, `col.all(db)`,
      `col.search_by(db, i, bound)`, `col.history(db, id)` (the trait gained
      `record_history` for that last one).
- [x] `secondaries.rs` `by_lookups`: `col.by_username(db, &str)` — the
      `{Name}Secondaries` trait now implemented for `CollectionHandle<T>`,
      methods take `&D`, items yield `T` (no `(Id, T)` tuple — walk-shaped
      ops yield values; ids come from `insert`).
- [x] `exec_ops.rs` decoupled first: the steps now drive
      `::wavedb_core::Collection::<#name>::at(pivot, tenant)` directly, so
      the wire ops never depend on the generated wrappers' shape.
- [x] Migrated every call site: schema-smoke tests, storage's
      `nonunique_collection.rs`, the node-side pivot seeding in
      `node_http.rs` / `client_e2e.rs` — spelling is
      `T::get(&LocalHandle::new(&store, tenant))` etc.
- [x] Deliberate non-goal: `record.save(&db)` on a NonUnique **value** (the
      README's spelling) stays out — a decoded value carries no `Id`, so
      handle-based `col.save(db, id, v)` is the M4 surface; identity-carrying
      records are a later design.

## T3 — `Db` + `ServerDb` implement `DbHandle` — **DONE (2026-07-06)**

- [x] `wavedb/src/client_handle.rs` (new): `impl DbHandle for Db`
      (`Error = wavedb::Error`) — frame sends moved in from
      `unique.rs` / `collection.rs`; walks fetch the buffered reply then
      replay as a stream per T1. Wire-less ops (`create_pivot`, `search_by`,
      `record_history`) refuse with the node's uniform
      `UnknownStructHash`. New `wire::to_wire_pair(&a, &b)` encodes the
      `(pivot, value)` / `(id, value)` payload tuples from borrows
      (byte-identical to the tuple encoding — no `Clone` bound on `T`).
- [x] `wavedb/src/server_db.rs`: `impl DbHandle for ServerDb<'_, S>`,
      internally a wrapped `LocalHandle`. `#[server]`'s `&Db → &ServerDb<S>`
      retyping stays; the generated body now also imports `DbHandle as _`
      so `db.as_tenant(..)` / `db.tenant()` trait spellings work inside.
- [x] Retired the interim surfaces: `db.get::<T>()` / `db.save::<T>()` /
      `db.collection::<T>()` / `ClientCollection` / `ServerCollection`
      deleted (`unique.rs` / `collection.rs` removed). `prelude` re-exports
      `DbHandle` + `CollectionHandle`.
- [x] `tests/client_e2e.rs` + `tests/server_fn_e2e.rs` rewritten to the
      unified spelling (`AboutUser::get(&db)`, `me.save(db)`,
      `Note::collection(pivot)` + `col.insert(&db, v)`), proving one body
      text works against `Db`, `ServerDb`, and `LocalHandle`. The `History`
      wire entries now carry `(Metadata, T)` pairs (core
      `unique_history_values` + client `reply::pairs`), so the remote
      timeline sees the chain, not just bodies.

## T4 — `store`-only exposure entries — **DONE (2026-07-06)**

- [x] `expose.rs`: new entry kind `store Path` (contextual keyword — a
      struct literally named `store` still parses) — contributes the type's
      `storage_entries()` to the emitted `StorageRegistry` impl and nothing
      else (no dispatch arms, `knows` = false, wire refusal unchanged);
      `expose_client!` rejects `store` entries (no engine client-side).
      Declaration grammar split into `expose_parse.rs` for the file budget.
- [x] schema-smoke proof: `store Attachment` — its slot rides
      `REGISTRY.storage_entries()`, `knows` stays false, and an execute
      naming its hash refuses `UnknownStructHash` like a type that never
      existed. (The fn-body read/write over a store-entry engine is T5's
      todo-app integration.)

## T5 — todo-app end-to-end (the M4 exit) — **DONE (2026-07-06)**

- [x] `examples/todo-app` is in the workspace (three member crates; the
      nested `[workspace]` and the root `exclude` entry are gone).
- [x] Schema against the real surface: `expose_server!` lists the six `fn`s
      + five `store` entries; `complete_todo` uses `col.save(db, id, &todo)`
      (T2 non-goal); `all_todos` returns `Result<Vec<Todo>>` buffered until
      T6 (`async_stream` dep dropped); helpers (`ensure_registry`,
      `get_profile`) are **`DbHandle`-generic** — the seam working as
      designed; sha256/timestamp auth stays (real auth = M8). New wire-crate
      impl: `()` is `WaveWire` (zero bytes) so `Result<()>`-returning fns
      wire their return.
- [x] Server main = `Server::new(REGISTRY).data_dir(dir).serve(addr)` (the
      aspirational `QuickNode::builder()` spelling is dead).
- [x] Client main: `127.0.0.1:7700` over HTTP POST, `U48` tenants,
      register → login → reconnect-as-tenant → add/list/complete/delete.
- [x] Integration proof (`examples/todo-app/schema/tests/e2e.rs`, single
      `#[tokio::test]`, node on its own thread): register + duplicate-name
      refusal, login + wrong-password refusal via the username secondary,
      `as_tenant` bootstrap, the profile→pivot path, tenant isolation, and
      the whole state surviving a node restart. The real server + client
      binaries also run the printed flow end-to-end.
- [x] Docs settle: this file's intro/DOING updated; exit recorded in
      `todo_done.md`.

## T6 — streaming reads over the transport — **DONE (2026-07-06)**

- [x] The response is now a sequence of length-prefixed frames
      (`[len u32 LE][StreamFrame wire]`; `Item(bytes)* End(Response)`)
      written progressively into the one POST body — no `content-length`,
      `connection: close` delimits, no chunked encoding. `http::FrameReader`
      reads them incrementally; `NetClient::call` (scalar: bare `End`) +
      `call_stream` (items as the node flushes them; a mid-walk fault
      arrives as a trailing `Error::Node` after the items already shipped).
- [x] Node side: `serve` unpacks a `Reply::Values` into one flushed `Item`
      frame per record + `End`. (`execute` still buffers internally — a
      later engine change behind the same wire.) Client
      `DbHandle::all`/`unique_history` decode item frames as they arrive
      (T1 signatures unchanged — internals only, as designed);
      `reply::values`/`pairs` deleted.
- [x] **Stream-returning `#[server]` fns**: `-> impl Stream<Item =
      Result<T>>` is detected (`server_stream.rs`); the body returns the
      stream against `ServerDb`, dispatch collects + ships items, and the
      client stub re-exposes the same async iterator over
      `Db::call_fn_stream`. The return hashes as its whole shape (a scalar
      and a stream of the same item are different functions).
- [x] `all_todos` returns `impl Stream<Item = Result<Todo>>` again — e2e +
      the real binaries run over the framed wire. Fallout fix:
      `CollectionHandle`'s stream methods use precise capture
      (`+ use<'d, D, T>`) so `T::collection(p).all(db)` works on a
      temporary handle under edition-2024 capture rules.

## T7 — composed function identity — **DONE (2026-07-07)**

- [x] Replaced the signature-string fn hash with the designed composition:
      `core::fn_identity::compose(name_seed, [arg tags…, return tag])` — an
      argument `#[wavedb]` struct tags as its `STRUCT_HASH` (macro-emitted
      `FnArgTag`), so a schema change to it transitively renames every
      function whose signature carries it. A stream return composes under
      `STREAM_KIND` (scalar vs stream of the same item = different fns).
- [x] The `const` composition path: `fn_identity` — a documented distinct
      const mixer (SplitMix64 folds, **not** seahash: must run in `const`
      context from other crates' consts; identity-load-bearing all the
      same, pinned by tests), `FnArgTag` fixed tags for the builtins
      (`u64`, `String`, `Id`, `U48`, …) and composing impls for
      `Vec`/`Option`/arrays/tuples. Decision documented at the module head
      and in `server.rs::composed_identity`.
- [x] Test `wavedb/tests/fn_identity.rs`: name seed, arg type, arity/order,
      scalar-vs-stream all separate identities; `Payload::TAG ==
      Payload::STRUCT_HASH` proves the transitivity contract.

# PLAN — M2 tail (storage engine)

Grounded in the code as of 2026-07-07. Today's model: reads serve **only**
from the per-type caches (the whole dataset lives in RAM); every `open`
truncates `data.bin` to its superblock and replays the **entire** journal
through the live commit+settle path. Correct, but the journal grows
unbounded and startup is O(history). The goal: `data.bin` becomes an
authoritative checkpoint so the journal truncates and open replays only the
tail. Dependency chain: S1 → S2 → S3 → S4; S5/S6 independent after S1.
Each task lands green (fmt + clippy + tests + file gate) and moves to
`todo_done.md` prose when done.

## S1 — page-backed reads (cache becomes a cache) — **DONE (2026-07-07)**

- [x] `PageStore::read_from_pages` (needs the `BlockFile`, so it lives on
      the store, not the slot): `get_of` serves the cache and falls through
      to `Directory::get_record` on a miss; untyped `get` probes every
      slot's cache first, then every slot's pages. An absent id costs one
      page probe — noted as fine until a keyed filter earns its place.
- [x] `Write::Remove` owner routing survives eviction: `owner_of` probes
      caches, then settled pages, under the journal lock (probe-then-mutate
      can't race — writers serialised). `commit_to_caches` is now fallible;
      a page-probe fault after the durability point under-applies live
      state but the journal holds the batch whole (documented). Lock order
      extended: `journal → dir → cache` on commit — still acyclic.
- [x] Tests: `evicted_records_read_through_from_pages` (typed + untyped +
      absent), `remove_of_evicted_record_reaches_its_page` (live + replay).
      Settle path split to `settle.rs` for the file budget (the S4 drain
      task lands there).

## S2 — checkpoint: persist the projection, truncate the journal — **DONE, then SUPERSEDED by J1–J5 (superblock-pointer checkpoint replaced by the journal-rooted commit)**

- [x] `checkpoint.rs`: a checkpoint block run holds
      `[len u32][to_wire_checked(CheckpointBody)]` — per settled type
      `(struct_hash, directory slots, dict run descriptor)` + the
      allocator's `total_blocks`. The superblock gained a `checkpoint:
      BlockDescriptor` field; repointing it (one durable block-0 rewrite)
      is the atomic commit. No journal offset needed: a checkpoint always
      covers the *whole* journal and truncates it to zero — a crash before
      the truncate replays covered frames over checkpoint state, which
      converges (settle writes cache state, idempotent; proven by test).
- [x] `PageStore::checkpoint()`: journal lock held throughout (writers
      quiesce, reads proceed); pages already current (settle inline) —
      sync data, write run, sync, repoint superblock, retire the old run,
      truncate journal. **Allocator protection** (`alloc.rs`, split from
      `block.rs` for budget): runs the durable checkpoint points at defer
      their frees (`set_protected` + pending list) so a crash mid-window
      never reopens onto overwritten pages; `from_layout` rebuilds the free
      map as the complement of the persisted runs.
- [x] Tests: checkpoint → cold reopen serves all + journal empty; stale
      (un-truncated) journal converges; corrupt checkpoint refuses;
      dictionary-compressed pages readable after restore; post-checkpoint
      writes replay over it; allocator protection + layout-rebuild units.

## S3 — fast open: load the checkpoint, replay the tail — **DONE, then SUPERSEDED by J1–J5 (recovery now roots in the newest `Commit` frame)**

- [x] `open` skips the `data.bin` truncate when `checkpoint::restore`
      finds a committed checkpoint: directories/dicts load into the slots,
      allocator from `from_layout` + protected set, caches stay **empty**
      (S1 read-through serves), and the (post-truncate) journal replays
      through the normal commit+settle path.
- [x] No checkpoint ⇒ the full-rebuild path unchanged. Corrupt checkpoint
      run ⇒ `Corrupt`, refuse — no silent fallback (FORMAT_VERSION
      policy). A checkpoint naming an unregistered type ⇒
      `UnregisteredStructHash` (allowlist holds at restore too).
- [x] Cold-open assertions: `cache_len() == 0` after a checkpointed
      reopen while every record reads; startup replay is just the tail.

## S4 — background settle + checkpoint policy — **DONE (2026-07-07)**

- [x] Settle off the `apply` hot path: `apply` = route → journal fsync →
      cache commit → merge touched into a `pending` queue (push under the
      journal lock, so "queue empty" is race-free); `PageStore::drain`
      settles rounds until empty (a failed round re-queues — idempotent).
      `split_next` rides the drain. **Unsettled-remove tombstones**: a
      `Remove` marks the id dead in its slot until the page rewrite lands,
      so the read fallback never resurrects stale page bytes (a re-`Put`
      clears it) — the hazard deferred settle introduces, caught + tested.
- [x] Node maintenance task (`quick-node::maintain`, spawned on the serve
      `LocalSet`): every 200 ms drain → checkpoint if
      `journal_len() > checkpoint_after_bytes` (builder, default 64 MiB) →
      `evict_settled(cache_budget_bytes)` (builder, default 1 GiB). Clean
      shutdown drains + checkpoints, so a restart replays nothing.
- [x] Eviction mechanism: `evict_settled` quiesces writers (journal lock),
      refuses while anything is pending, then evicts settled entries down
      to budget (`StructStorage::evict_up_to`); evicted reads fall through
      to pages.
- [x] Tests: unsettled write reads from cache + survives reopen
      (journal); tombstone hides stale page until settle + re-put
      supersedes; eviction refuses while pending, evicts to budget after;
      checkpoint drains first (existing checkpoint tests exercise it).

## S5 — dedicated 32 KiB one-node-per-page BpTree format — **DROPPED (2026-07-07, user decision)**

The README spec predates tenant partitioning of the trees. A `BpTree` is
**per tenant**, so a B2C node hosts millions of *small* trees — one 32 KiB
page per node wastes ~32 KiB for a tenant with a handful of records,
exactly the dominant case. The shared linear-hash `SlotPage` path packs
many tenants' small nodes into common buckets and the split threshold
bounds page size; node values also rewrite on every index mutation, so any
one-tree-co-location scheme churns immediately. Revisit only if a
measured workload shows single huge trees dominating cold reads. (README
spec section needs a matching `> Status:` note.)

## S6 — per-value heap compression (last, optional)

- [ ] Large string/blob heap segments compress individually inside the
      record envelope; page-level zstd already covers the common case —
      only worth landing if measured wins on real payloads.

# PLAN — journal-rooted recovery (replaces the S2/S3 checkpoint)

User-directed redesign (2026-07-07): the journal's crc framing is the
engine's **only** atomicity mechanism. No superblock rewrites, no
checkpoint run, no pointer files. `data.bin` pages + directory chains are
CoW; a single `Commit` frame in the *new* journal retires the old one.
S2/S3's superblock-pointer checkpoint is superseded (S1 read-through and
S4 drain/tombstones/eviction survive unchanged).

Model: writes append `Batch` frames to `journal_<ts>.log` (fsync = client
ack) + cache. Rotation at threshold creates `journal_<ts2>.log` and
redirects appends (no write lock). Commit of the old journal: drain
everything → write every type's directory as CoW **chain blocks**
(`{next u64, prev u64, addresses Vec<u64>}`, 0 = null — block 0 is the
write-once superblock) → append ONE `Commit { journal_ts, roots: all
registered hashes, dicts }` frame (all roots, not just touched — deleting
the old journal must not lose untouched types' tracking) → fsync → delete
old journal → release deferred CoW frees. The Commit frame is appended
only after every step, under the journal append lock — a concurrent
`Batch` fsync makes everything before it durable (physical order is the
contract). Recovery: scan `journal_*.log` sorted; newest valid `Commit`
gives the roots base; allocator derives from the chains + pages; every
`Batch` in the remaining journals replays (re-settle converges — proven).
A `data.bin` with no journal present is corrupt (refuse).

## J1 — timestamped journal + rotation — **DONE (2026-07-07)**

- [x] `journal_<nanos>.log` naming; open scans the dir (fresh dir creates
      the first; `data.bin` present with no journal = refuse). `rotate()`
      creates + redirects appends without blocking writers.

## J2 — typed frames — **DONE (2026-07-07)**

- [x] `JournalFrame { Batch(Vec<Write>), Commit { journal_ts, roots:
      Vec<(u64, u64)>, dicts: Vec<(u64, u64)> } }`, WaveWire inside the
      existing `[len][crc]` framing; torn/invalid tail truncation
      unchanged.

## J3 — directory chain blocks (CoW, in `data.bin`) — **DONE (2026-07-07)**

- [x] Encode a type's `Directory.slots` (+ occupation, raw descriptors) as
      linked 4 KiB blocks **in `data.bin`**; load walks next/prev. The
      journal only ever carries the 8-byte root address. A type whose
      directory did **not** change since the last commit rewrites nothing —
      the new `Commit` frame repeats its previous root address (the real
      per-rotation saving); only touched types write a fresh CoW chain.
      Dictionary runs stay as today, rooted from the Commit frame's
      `dicts`.

## J4 — commit flow + policy — **DONE (2026-07-07)**

- [x] `PageStore::commit_journal()`: rotate → drain all → write fresh
      chains for the touched types only → append ONE `Commit` frame
      (roots for **all** registered hashes — 16 B each; untouched types
      repeat their old address) → fsync → delete old journal → release
      deferred frees (CoW blocks retired by this commit). Maintenance
      task rotates at `checkpoint_after_bytes`; clean shutdown commits.
      Nothing referenced by the latest durable Commit is ever
      overwritten in place; frees defer until the covering Commit is
      durable.

## J5 — recovery + superblock revert + tests — **DONE (2026-07-07)**

- [x] Open: sorted journal scan, newest valid `Commit` = roots base
      (committed-but-undeleted old journal is skipped), allocator from
      chains + pages + dict runs, replay remaining `Batch` frames.
      Superblock reverts to write-once (checkpoint field removed);
      `checkpoint.rs` run/pointer machinery deleted.
- [x] Crash-window tests: torn `Commit` frame (old journal still rules);
      crash after Commit before delete (old journal skipped); multi
      rotation; untouched-type roots survive a commit that never touched
      them; cold open reads via chains.

# PLAN — M7 live sync

Grounded in the code as of 2026-07-11. Exit: **client A saves; client B's
watcher fires within one round-trip (WS) / one poll tick (HTTP)**.
Dependency chain: W1 → W2 → W3 → W4 → W5 (the WS half of the exit);
W6/W7/W8 follow independently. Each task lands green (fmt + clippy + tests
+ file gate) and moves to `todo_done.md` prose when done.

Standing decision: WebSocket is **hand-rolled RFC 6455**, same stance as
the HTTP tunnel — binary messages only as API, no extensions, no
subprotocols; the workspace's Phase-11 `tokio-tungstenite` / `gloo-net` /
`axum` declarations stay unused exactly like `reqwest` did when the POST
tunnel was hand-rolled. Sharing the POST port (routing on `GET` + `Upgrade`
in the same head parser) falls out for free. One new dep: `sha1`
(RustCrypto, the family `hmac`/`sha2` already come from) for the RFC 6455
accept key; base64 is a ~20-line encode (encode-only, no alphabet variants).

## W1 — WebSocket primitives (`wavedb-platform::ws`) — **DONE (2026-07-11)**

- [x] Frame codec (`ws/codec.rs`, native, shared with the server half):
      read/write over `AsyncRead`/`AsyncWrite` — binary/continuation
      reassembly (browsers may fragment), ping/pong, close; client→server
      frames masked, server→client unmasked (per RFC — the server refuses
      unmasked data frames); payload cap `MAX_MESSAGE`. `accept_key` +
      an encode-only `base64` (no dep). One new dep: `sha1`.
- [x] `ws::connect(addr) -> Conn` cfg-switched like `http::post`:
      native = fresh `TcpStream` + client handshake (key from platform
      entropy, `Sec-WebSocket-Accept` verified); wasm = `web-sys`
      `WebSocket` (`binaryType = arraybuffer`, events bridged to a
      `futures::channel` — the `idb.rs` closure pattern, closures held for
      the connection). Same surface both targets: `send(bytes)` /
      `recv() -> Option<Vec<u8>>` / `close()`; `recv` answers pings
      internally.
- [x] Unit tests: RFC 6455 §1.3 accept-key worked example, base64
      alignment vectors, masked roundtrip across all length forms,
      fragmented reassembly across a ping, oversized refusal, unmasked
      refusal, clean-close-as-`None`; native loopback echo + handshake +
      bad-accept + non-upgrade tests.

## W2 — WS envelopes + server upgrade (`wavedb-net`) — **DONE (2026-07-11)**

- [x] Wire messages (`ws.rs`, target-independent):
      `ClientMsg { Hello(Auth) | Call(CommandFrame) | Subscribe(Topic) |
      Unsubscribe(Topic) }`, `ServerMsg { HelloOk | Item(Vec<u8>) |
      End(Response) | Event(RecordEvent) }`,
      `Topic { struct_hash, pivot: Option<LocalId> }`,
      `RecordEvent { topic, id, kind: Saved|Removed, body }`. The tenant
      never rides a topic — it is the connection's bound identity.
- [x] Server half: the head parser learns `GET` + `Upgrade` (new
      `read_request -> Incoming::{Post, Upgrade{key}}`; POST path
      byte-identical, `read_post` retired), `write_switching_head`
      (101 + accept key); frames pipelined before the 101 are refused.

## W3 — mutation notifications (core seam) — **DONE (2026-07-11)**

- [x] `Store::note_mutation(&self, impl FnOnce() -> Mutation)` — a
      **provided no-op** on the trait (the closure never runs unless a
      store overrides it, so ordinary stores never even build the value);
      `Mutation { struct_hash, tenant, pivot: Option<LocalId>, id,
      kind: Saved|Removed, body }` in `core::notify`. Called after the one
      atomic `apply` in `save_unique_as`, `insert_at`, `Collection::save`,
      `Collection::remove` — the chokepoint every mutation crosses,
      **including `#[server]` bodies** (they write through `ServerDb` →
      the same collection layer). Batch-derivation was considered and
      rejected: a chained save writes up to three same-type record `Put`s
      (live + fresh archive + repointed archive, metadata-indistinguishable)
      and a remove may rewrite no record at all — semantics live above the
      batch. Also landed: blanket `impl Store for Rc<S>` (a shared store is
      a store) so a wrapper can own its backend by value while a
      maintenance handle keeps its own clone.
- [x] Cache mirrors (`Collection::adopt`, the client cache) ride the
      default no-op — a mirrored write is not a mutation event. Tests prove
      one event per op, the anchor id for a Unique save, and **nothing** on
      a failed `apply` or a no-op re-remove.

## W4 — node: subscriptions + push (`wavedb-quick-node`) — **DONE (2026-07-11)**

- [x] `SubTable` + `NotifyStore<S>` (`subscribe.rs`) — concrete wrapper
      (no `dyn`) forwarding `get`/`get_of`/`apply`, overriding
      `note_mutation` to route into a `Rc<RefCell<SubTable>>` keyed
      `(tenant, Topic)` → per-connection `ServerMsg` senders
      (O(subscribers-of-this-topic), exact match, no scan; dead senders
      pruned on publish). `Bound` keeps the raw `Rc<PageStore>` for
      maintenance/seeding/commit and builds the `NotifyStore` per-serve
      around a clone of that `Rc`.
- [x] WS session loop (`serve_ws.rs`): 101 handshake → first message must
      be `Hello` (gate 1 `identify` via the extracted `dispatch::execute`
      shared with HTTP, else refuse + close) → `select!` over a reader
      task's decoded messages (frame reads aren't cancel-safe) and the
      event channel. `Call` runs gates 2–3 + `Item*/End` (FIFO per
      connection); `Subscribe`/`Unsubscribe` mutate the table under the
      **caller's** tenant (anonymous callers subscribe to nothing).
      Disconnect unregisters every subscription of the connection.
- [x] **Proven** (`tests/node_ws.rs`, one process/one engine, node on its
      own thread): two identity-bound connections over real RFC 6455 frames
      (native `ws::connect`) — `Hello`→`HelloOk`, a `Call` walk, a Unique
      save + a collection insert on one connection push exact-topic
      `Event`s (right id, decoded body) to the other's declared
      subscriptions, and an `Unsubscribe` provably stops one topic while
      the other still fires.

## W5 — client watch + cache sync — **DONE (2026-07-13)**

- [x] `WsSession` (net, both targets): platform `ws::connect` + `Hello` →
      `HelloOk`, `subscribe(topic)`, `next_event()`. Landed with a protocol
      addition: `ServerMsg::TopicOk` acks `Subscribe`/`Unsubscribe` (FIFO ⇒
      the ack proves the table mutation is live), events racing an ack are
      buffered, and an anonymous `Subscribe` closes the connection instead
      of being silently ignored. Loopback unit tests (ack buffering,
      refused hello).
- [x] `Db::watch_unique::<T>()` / `Db::watch_collection::<T>(pivot)` —
      each watch owns one WS connection (multiplexing = later refinement);
      events **mirror into the M6 cache** (`mirror_unique` /
      `mirror_record` / `mirror_remove`) before yielding, so a watcher
      keeps the local store warm — the live half of sync. Token-less
      handles refuse typed (`Unauthorized`). Typed `T::watch(&db)` sugar
      stays deferred — joins the `T::get(&db)` unification note (watch is
      `Db`-only; no `DbHandle` seam for it yet).
- [x] **Exit (WS half) e2e**: two clients on one node
      (`examples/contact-book/tests/live_watch_e2e.rs`, node as a child
      process) — A saves a Unique + inserts/updates/removes in a
      collection; B's watchers see each event typed, in order, under
      node-minted ids; after a node kill B's cache answers warm — including
      the full walk of a collection B never once read online (the watcher's
      mirrors alone built it).

## W6 — journal cursor catch-up (reconnect)

- [ ] A monotone commit sequence per applied batch, carried on the push
      stream; reconnect = `Resume { since }` streaming exactly the
      tenant's deltas after it; a cursor older than the oldest retained
      journal answers "resync" (the client re-reads its subscriptions).
      Mechanism decision recorded when the task starts: replaying
      journal `Batch` frames needs the W3 disambiguation, so the cursor
      path likely logs `Mutation`s (or resolves live-vs-archive by id).

## W7 — HTTP piggyback + idle tick (POST clients)

- [ ] Per-session (token `session` id) event buffer node-side; the client
      queue sends empty polls at `http_poll_interval` when idle, backing
      off; responses piggyback buffered events. The HTTP half of the exit.

## W8 — offline write queue

- [ ] M6's refused offline writes become a durable local queue replayed
      through the W6 cursor path on reconnect (order kept, node-first
      semantics preserved: the queue drains before reads trust the node).
