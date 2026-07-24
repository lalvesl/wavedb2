# TO DO

Clean reimplementation of WaveDB. The docs describe the **target** design;
everything that has landed is in [`todo_done.md`](todo_done.md). Workspace
members today: wire, wire-derive, core, macros, storage, net, quick-node,
wavedb, schema-smoke, todo-app (schema/server/client). Excluded (not built
yet): wasm, bench, test-cluster. Remaining work, bottom-up (the task log is
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

- **IndexedDB `Store`**: key = 128-bit `Id` (big-endian), value = wire bytes;
  `apply` = one IDB readwrite transaction; no pages, no journal (IndexedDB
  already does block management + crash safety);
- `wavedb` + `wavedb-net` compile for `wasm32`: browser `fetch` POST
  transport, `wasm_bindgen_futures` runtime — same async API;
- **no tokio inside wasm** (user constraint, 2026-07-07): tokio stays
  behind `cfg(not(target_arch = "wasm32"))` everywhere (it already is —
  net gates it, client/schema carry it as dev-deps only); the wasm build
  runs on `wasm_bindgen_futures`, keeping the binary small. The wasm side
  has **no journal and no `data.bin`** — IndexedDB `Id → Vec<u8>` is the
  whole store (the `Store` trait absorbs the difference);
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

_Workspace green: fmt + clippy (pedantic + nursery) clean, 29 test suites,
file-length gate passing. Members: wire, wire-derive, core, macros, storage,
net, quick-node, wavedb, schema-smoke, todo-app (schema/server/client).
Still excluded: wasm, bench, test-cluster._

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
