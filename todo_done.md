# DONE

- **M8 — auth & permission enforcement (2026-07-10)**: verified identity
  end to end, sessions with rotation + revocation, the `#[server]` guard
  live. Argon2 and record-level grants (gate 4, rides the deferred
  cross-tenant read path) stay open — see the M8 section in `todo.md`.
  - **Tokens** (`wavedb-net::auth`): `[to_wire(AccessClaims)][HMAC-SHA256]`;
    claims `{ user, tenant, expires_at, purpose: Access|Refresh, session,
    nonce }`. `sign`/`verify` (constant-time MAC first, then expiry, then
    purpose — a refresh token can never pass as access). The `nonce` exists
    because rotation compares token *hashes*: two pairs minted in the same
    second must differ byte-for-byte.
  - **Envelope**: `Request { auth, frame }` with
    `Auth::Anonymous { tenant } | Auth::Token(bytes)` — identity inside the
    body, transport stays a dumb tunnel. `NodeErrorKind::Unauthorized` is
    the one uniform refusal (which check failed stays server-side).
  - **Gate 1** (`quick-node::dispatch::identify`): token → verified
    `Caller { user, tenant }` (claimed tenant ignored); anonymous →
    `Caller::anonymous` (`user = U48::MAX`). Secret: `Server::secret(..)`
    or random per boot; installed once process-wide
    (`wavedb_net::auth::set_node_secret`) so the minting helpers and an
    in-process reopen agree.
  - **Caller threading**: `Exposure::execute(store, caller, ..)`; generated
    `__wavedb_*` steps take `Caller` and refuse the anonymous tier before
    any work; fn dispatch builds `ServerDb::for_caller`. Authorship:
    `Metadata.user` = the verified user (`Collection::stamped_by`,
    `save_unique_as`) — engine-local paths keep `user = tenant`.
  - **Guard in the macro**: `#[server]` injects the anonymous refusal ahead
    of arg decoding; `#[server(public)]` skips it. Body errors now flatten
    via `Into<core::Error>` so `Error::Unauthorized` keeps its identity on
    the wire (was: everything stringly `Backend`).
  - **Sessions** (`wavedb::auth`): NonUnique `AuthSession { user,
    refresh_hash, issued, revoked }` + Unique `AuthSessions` anchor, lazily
    bootstrapped in the session tenant's space; refresh claims carry the
    record's raw `Id`. `issue_pair` → `TokenPair` (15 min access / 30 day
    refresh); `refresh_pair` rotates the stored hash (replay ⇒ revoke);
    `revoke` = logout. Tokens stored only as sha256 — reading the store
    never yields a usable token.
  - **Client**: `Db::with_access_token(bytes)`; every request ships
    `Auth::Token`, else `Auth::Anonymous` under the claimed tenant.
  - **Proofs**: todo-app e2e (login pair → work under token; anonymous
    non-public refused; replayed refresh revokes the session; logout kills
    refresh; all survives restart); dispatch unit tests (expired / forged /
    wrong-purpose / anonymous-tier); the M4/M3 e2es re-signed with real
    tokens. `record.rs` split (`record_unique.rs`) for the file budget.

- **Journal-rooted recovery — J1–J5 (2026-07-07, user-directed design)**:
  the journal's crc framing is the engine's **only** atomicity mechanism;
  the S2/S3 superblock-pointer checkpoint (same day) was superseded.
  - **Timestamped journals** (`journal_<nanos>.log`): rotation creates a
    new file and redirects appends under the lock for microseconds — no
    write blocking during a commit. `data.bin` present with no journal
    refuses to open (recovery root lost).
  - **Typed frames**: `JournalFrame::{Batch(Vec<Write>), Commit}` inside
    the existing `[len][crc]` framing. `CommitFrame { journal_ts, roots,
    dicts }` — ONE frame, atomic by its own crc (torn ⇒ invisible ⇒ the
    retired journal still on disk rules). Roots list **every** registered
    type (16 B each) — untouched types repeat their old address, so
    deleting the old journal never loses tracking.
  - **Directory chains** (`chain.rs`): a type's page addresses persist in
    `data.bin` as CoW linked 4 KiB blocks `{next, prev, addresses}`
    (0 = null; block 0 is the write-once superblock). The journal carries
    only the 8-byte root. Only dirty types (tracked per-slot in the
    compile-time `ChainTrack`) rewrite their chain at commit.
  - **Commit flow** (`commit.rs`): rotate → drain → CoW chains for dirty
    types → data sync → append `Commit` (under the append lock — a
    concurrent Batch fsync makes prior bytes durable, physical order is
    the contract) → delete old journal → roll the allocator's protected
    set forward (frees deferred under the previous commit release).
  - **Recovery**: scan sorted journals, newest decodable `Commit` = base;
    covered leftovers (crash between commit and delete) are skipped and
    cleaned; allocator derives from chains + pages + dict runs
    (`from_layout`); uncovered `Batch` frames replay (re-settle
    converges). Superblock reverted to write-once (checkpoint field
    removed). Crash-window tests: torn Commit falls back to the old
    journal; covered-journal leftover; multi-generation; dict-compressed
    pages restored.

- **M2 tail — background settle + checkpointing (S1–S4, 2026-07-07)**: the
  journal no longer grows unbounded; `data.bin` is a real recovery source.
  - **S1 page-backed reads**: the cache is a cache — `get`/`get_of` fall
    through to `Directory::get_record` on a miss (`read_through.rs`);
    `Remove` owner routing probes pages when no cache holds the id.
  - **S2 checkpoint** (`checkpoint.rs`): one block run persists every
    settled type's directory slots + dictionary run + `total_blocks`; the
    superblock gained a `checkpoint: BlockDescriptor` — repointing block 0
    is the atomic commit, then the journal truncates to zero (a checkpoint
    always covers the whole log; replaying a stale log over checkpoint
    state converges). Allocator protection (`alloc.rs`): the durable
    checkpoint's runs defer their frees until the next commit, so a crash
    mid-window never reopens onto overwritten pages.
  - **S3 fast open**: with a checkpoint, no `data.bin` truncate — restore
    directories/dicts/allocator (`from_layout` = complement of persisted
    runs), caches start cold, replay only the journal tail. Corrupt
    checkpoint refuses (`Corrupt`); unregistered type refuses
    (`UnregisteredStructHash`).
  - **S4 deferred settle + policy**: `apply` = journal fsync → cache
    commit → `pending` queue (pushed under the journal lock);
    `PageStore::drain` settles rounds idempotently; unsettled removes
    tombstone their id so read-through never resurrects stale page bytes.
    quick-node `maintain` task (200 ms on the serve `LocalSet`): drain →
    checkpoint past `checkpoint_after_bytes` (64 MiB default) →
    `evict_settled` to `cache_budget_bytes` (1 GiB default); clean
    shutdown drains + checkpoints so a restart replays nothing.
  - **S5 dropped (user decision)**: 32 KiB one-node-per-page BpTree format
    predates tenant partitioning — trees are per tenant, B2C = millions of
    small trees, a page per node wastes the dominant case; nodes stay in
    the shared linear-hash `SlotPage` buckets. README carries the Status
    note. S6 (per-value compression) deferred pending measurement.

- **Streaming wire + composed function identity — T6/T7 (2026-07-06/07)**:
  the post-M4 refinements, closing the PLAN.
  - **Framed streaming responses (T6)**: the HTTP response body is a
    sequence of `[len u32 LE][StreamFrame wire]` frames —
    `Item(bytes)* End(Response)` — written progressively (no
    `content-length`; `connection: close` delimits). `http::FrameReader`
    reads incrementally; `NetClient::call` (scalar = bare `End`) +
    `call_stream` (a mid-walk fault ships as a trailing `Error::Node`
    after items already sent). `serve` unpacks `Reply::Values` into one
    flushed `Item` per record; `execute` still buffers internally — a
    later engine change behind the same wire. Client `DbHandle::all` /
    `unique_history` decode frames as they arrive; `reply::values`/`pairs`
    deleted.
  - **Stream-returning `#[server]` fns (T6)**: `-> impl Stream<Item =
    Result<T>>` detected (`server_stream.rs`); the body *returns* the
    stream against `ServerDb`, dispatch collects and ships items, the
    client stub re-exposes the same async iterator via
    `Db::call_fn_stream`. `CollectionHandle` stream methods use precise
    capture (`+ use<'d, D, T>`) so walks on a temporary handle compile
    under edition-2024 capture rules.
  - **Composed function identity (T7)** (`core::fn_identity`): a fn's
    `STRUCT_HASH` = `compose(name_seed, [arg tags…, return tag])`. The
    `FnArgTag` trait gives every signature type a `const` 64-bit tag:
    `#[wavedb]` structs tag as their `STRUCT_HASH` (schema evolution
    transitively renames the function), builtins carry fixed tags,
    `Vec`/`Option`/arrays/tuples compose their element's, a stream return
    composes under `STREAM_KIND` (scalar ≠ stream of the same item). The
    mixer is a documented distinct `const fn` (SplitMix64 folds — seahash
    is not `const`); identity-load-bearing, pinned by
    `wavedb/tests/fn_identity.rs` (name/arg/order/arity/stream-vs-scalar
    all separate; `Payload::TAG == Payload::STRUCT_HASH`).

- **M4 COMPLETE — the `T::get(&db)` unification + todo-app end-to-end
  (2026-07-06)**: the documented spelling is real and one body text runs
  against every execution context.
  - **core `DbHandle` seam** (`handle.rs`): the trait all three contexts
    implement — `type Error: From<core::Error>`, `tenant`/`as_tenant`, the
    full op set (`get_unique`/`save_unique`/`unique_history`,
    `create_pivot`, `insert`/`get_record`/`update`/`remove`/`all`/
    `search_by`/`record_history`). Walk-shaped ops return `impl Stream` in
    the trait signature with `T: 'static` (free — `WaveWire` values are
    owned), so the buffered M4 client can go streaming later without
    touching call sites. `LocalHandle<'a, S: Store>` = the engine-local
    impl. Fallout fix: `Collection`'s read methods take `self` by value
    (`Copy` handle; edition-2024 RPIT capture rules tied streams to
    temporaries under `&self`).
  - **macro re-plumb**: `#[wavedb]` now emits `T::get(&db)` / `value.save(&db)`
    / `T::history(&db)` (Unique) and `T::collection(pivot) ->
    CollectionHandle<T>` / `T::create_pivot(&db)` (NonUnique) — all generic
    over `DbHandle`; `{Name}Secondaries` (`by_<field>(db, ..)`) is
    implemented for `CollectionHandle`; walk-shaped surfaces yield values
    (ids come from `insert`). The exec steps decoupled onto
    `Collection::at` first, so wire ops never depend on the wrappers' shape.
    Non-goal recorded: `record.save(&db)` on a decoded value stays out (no
    identity on the value) — `col.save(db, id, v)` is the surface.
  - **`Db` + `ServerDb` implement `DbHandle`**: the client turns ops into
    command frames (`to_wire_pair` encodes payload tuples from borrows —
    byte-identical to tuple encoding, no `Clone` bound); wire-less ops
    (`create_pivot`, `search_by`, `record_history`) refuse with the uniform
    `UnknownStructHash`. `ServerDb` wraps a `LocalHandle`; the `#[server]`
    body gains a `use DbHandle as _` so trait spellings work inside. The
    interim `db.get::<T>()` / `ClientCollection` / `ServerCollection`
    surfaces are deleted. `History` wire entries now carry `(Metadata, T)`
    pairs, so a remote timeline sees the chain.
  - **`store` exposure entries**: `expose_server! { …, store Credentials }`
    registers a type's engine slots with **zero** wire surface (hash refuses
    like a type that never existed); `expose_client!` rejects them. The
    functions-only app shape needs nothing else.
  - **todo-app = M4 exit, proven**: three workspace member crates; the six
    `#[server]` functions are the whole wire API, every struct a `store`
    entry; helpers are `DbHandle`-generic; `()` gained a `WaveWire` impl for
    `Result<()>` returns. E2E test: register (+ duplicate refusal via the
    username secondary), login (+ wrong-password refusal), `as_tenant`
    bootstrap, profile→pivot todos, tenant isolation, and full state
    surviving a node restart — plus the real server + client binaries
    running the printed flow.

- **Exposure (struct surface): `expose_server!` / `expose_client!`** — the
  declared registry is real:
  - **core `expose` module** — `Command` (`Get`/`Save`/`Insert`/`Update`/
    `Remove`, WaveWire), `Reply` (`Value`/`Inserted`/`Removed`/`Done`), and
    the `Exposure` trait (`knows` / `decode_check` / `async execute<S: Store>`
    — the node builder's `.registry(…)` bound; static dispatch, the client
    default refuses). **Every refusal is `UnknownStructHash`**: unlisted
    type, excluded op, and wrong-shape command are deliberately
    indistinguishable.
  - **`#[wavedb]` now emits the per-command execution steps** —
    `__wavedb_{get,save,insert,update,remove}` with the uniform exposure-op
    signature `async fn(&S, U48, &[u8]) -> Result<Reply>` on every type
    (wrong-shape ops refuse), defined at the item, reachable only when
    listed. NonUnique `update`/`remove` are **handle-less**: they reach the
    collection through the record's `Metadata.pivot_id` back-link (payloads:
    insert `(LocalId, body)`, update `(Id, body)`, remove/get `Id`, Unique
    save = body, get = empty).
  - **`expose_server!`** expands the list to a zero-sized `ServerRegistry` +
    `REGISTRY` const implementing `Exposure`: one `match` on the hash per
    operation, arms calling the generated steps — a per-op override
    (`save: audited_save`) substitutes the path **inside the arm** at
    expansion time, `never` yields the refusal arm. No `dyn`, no fn-pointer
    tables, no runtime registration. **`expose_client!`** emits
    `ClientRegistry`/`CLIENT_REGISTRY` with the reachability half only
    (`knows` + `decode_check`; no overrides accepted, execute refuses) —
    typed call stubs land with `#[server]`/`Db` (M4).
  - Proven in `schema-smoke`: real declarations (submodule path entry,
    audited-save override observed firing, `get: never` exclusion), Unique
    save/get round-trip through the dispatch, the full NonUnique command set
    driving a live collection (update re-keys the `pinned` secondary via the
    metadata back-link), uniform unknown-hash refusals, client registry
    engine-less.
- **Per-record `Metadata` + the version chain (history)** — pillar 3 made
  real: saving never destroys old bytes.
  - **Record envelope v2** (`record.rs`): user records store as
    `[STRUCT_HASH][meta_len (u32 LE)][WaveWire(Metadata)][WaveWire body]`;
    `Pivot` records and BpTree nodes keep their meta-less forms. Decode splits
    metadata and body independently (`split_record` reuses raw body bytes so
    archiving never re-encodes a value).
  - **The chain**: a `save` archives the superseded version at a freshly
    minted id and links `Metadata` — live `old_modification_id` → newest
    archive; each archive backward to its predecessor; each archive's
    `new_modification_id` forward to the archive that superseded it (`None`
    on the newest = "successor is the live record", repointed in the same
    batch when the next save lands). One shared planner
    (`record::plan_chained_save`) serves Unique and NonUnique; the whole
    save — archive + relink + live write + secondary re-keys — is **one
    atomic batch**. `insert` stamps `Metadata.pivot_id` (the future
    handle-less `record.save` seam) and `user = tenant` (real authorship
    arrives with node auth, M8); permission carries forward across saves.
  - **Timeline API**: `Collection::history(store, id)` and the generated
    Unique `T::history(store, tenant)` (over
    `wavedb_core::record::unique_history`) stream `(Metadata, T)` versions
    newest-first. Reads (`get`/walks) still yield the value alone.
  - Proven: chain-shape assertions core-side
    (`save_archives_versions_and_history_walks`,
    `unique_save_chains_and_history_walks`), derived surface (schema-smoke
    `derived_unique_history_walks_versions`), durable engine
    (`version_history_survives_reopen` — the archives are ordinary journaled
    writes, replay reproduces the timeline).
- **Secondary indexes (`#[wavedb::pivot(...)]`) through `Collection`** — the
  M2 item, end to end:
  - **core `BpTree` generalised over its key** — `BpTree<K: NodeKey = LocalId>`
    (`NodeKey: Clone + Ord + Debug + WaveWire` + `record()` / `matches(bound)`
    / `may_intersect(bound, window)` for search + descent pruning). The
    primary tree is `K = LocalId` (unchanged semantics, tests untouched);
    secondaries use `SecKey { field: Vec<u8>, rec: LocalId }` — `IndexKey`
    field bytes major, record id breaking ties, so duplicate field values
    coexist and `Exact`/`Prefix`/`Range` bounds select by field. One
    machinery, monomorphized, no `dyn`. Node values share the reserved
    BpTree-node tag with a new `kind` byte (`[hash][kind][WaveWire payload]`,
    composed from the generic `Vec`/tuple wire impls — no bespoke codec).
  - **`Collection` maintains every tree**: `create` plans `current` + `dead` +
    one secondary per index (roots in the pivot via the widened
    `Pivot::replace_roots(current, dead, secondaries)`); `insert` indexes all;
    `remove` de-indexes all (record bytes supply the old keys); `save` re-keys
    only the indexes whose fields changed — old key out, new key in, **one
    atomic batch**, planned against an `Overlay` view (a batch-pending read
    layer in `record.rs`) so the second plan on the same tree sees the first's
    node writes (bug found by test: without it the later node rewrite undid
    the earlier). `search_by(index, bound)` walks a secondary two-phase;
    unknown index = `Error::SecondaryIndexOutOfRange`. Seams:
    `NonUniqueStruct::{NUM_SECONDARIES, secondary_key(i)}` (defaults keep
    hand-rolled impls valid); `Store::get_of` used throughout. `collection`
    split into `collection.rs` (handle + reads) + `collection_write.rs`
    (mutations) + `record.rs` (envelope, mint, Overlay, unique anchors —
    macro paths preserved via re-export) for the file budget.
  - **macro surface**: `#[wavedb::pivot(field)]` / `#[wavedb::pivot((f1, f2))]`
    (2–3 fields, validated against the struct, unknown field = compile error)
    now emit the key hooks **and a typed lookup trait** `{Name}Secondaries`
    implemented for `Collection<{Name}>` — `by_pinned(&store, &true)`,
    `by_customer_date(&store, &c, &d)`; `String` fields take `&str`. Static
    dispatch only. `save`'s semantics documented: re-key only changed indexes
    (the "force reindex all" wording in older docs is superseded — primary
    never re-keys, its key is the immutable `CREATED_AT`).
  - Proven at every layer: core (`secondary_tree_indexes_by_field_bytes`,
    `secondary_index_lifecycle`), derived surface (schema-smoke
    `derived_secondary_index_by_field`), durable engine
    (`secondary_index_survives_reopen`: re-key + remove survive journal
    replay).
- **`wavedb-wire`** — the `WaveWire` codec extracted into a standalone crate (only
  `thiserror`): trait + `Cursor` + builtin impls + `to_wire`/`from_wire` + its own
  `Error`. No `STRUCT_HASH`, registry, `Id`, or engine coupling — pure value ⇄
  bytes, decode fails only on a buffer/size mismatch (`UnexpectedEof`) plus
  intrinsic per-type checks. The trait is named `WaveWire` (renamed from `WaveWire`);
  trait + derive share the name like `Clone`. `wavedb-core` re-exports it as
  `wavedb_core::wire` **and directly** at the crate root (`wavedb_core::WaveWire`),
  and wraps its `Error` via `#[from]`, so every existing path keeps working.
- **`wavedb-wire-derive`** — the wire crate's own `#[derive(WaveWire)]` proc-macro
  (serde/serde_derive pattern; re-exported as `wavedb_wire::WaveWire`), emitting
  `::wavedb_wire::` paths. Supports structs (named/tuple/unit) **and enums** (the
  canonical tag form — `tag u8 [+ payload-len u32]`, declaration-order tags). Used
  to **replace the hand `WaveWire` impls** in core for `Id`, `LocalId`, `Metadata`,
  and `PermissionRef` (byte-identical — existing roundtrip/length tests pass
  unchanged). `U48` stays hand-written (6-byte 48-bit packing, not field-derivable).
  The older `wavedb-macros::WaveWire` (emits `wavedb_core::wire` paths, struct-only)
  is left for `#[wavedb]`; the two derives coexist by path target.
- **`wavedb-core`** — `Id`, `LocalId`, `U48`, `Metadata`, `PermissionRef`,
  `WaveWire` (re-exported from `wavedb-wire`, also at the crate root; the first four
  derive it, `U48` hand-written), `Error`. Plus the portable contracts: `WaveDbStruct` +
  `Shape`, `Store` (+ `Write`), `LookupHooks`,
  and the `index` layer — `IndexKey` (order-preserving), `Bound`, `Pivot`, `BpTree`,
  `IdStreamExt` (intersect/union/except stream adapters).
- **`wavedb-macros`** — `#[derive(WaveWire)]` (named/tuple/unit) and `#[wavedb]`
  (Unique/NonUnique): emits `STRUCT_HASH`, `WaveWire`, inherent consts
  (`SHAPE`/`HAS_VALIDATE`/`HAS_PREPROCESS`), `WaveDbStruct`, and
  for NonUnique the generated `{Name}PivotId` + `{Name}Pivot`. `#[wavedb::pivot(...)]`
  parsed/stripped → secondary-index count. `#[server]` deferred to M4 (needs `Db`).
  - **`STRUCT_HASH` uses SeaHash (pinned crate)** — portable across arch/endianness so
    client and server agree on identity; the crate is version-pinned so identity can't drift.
- **`wavedb-build` removed** — the `src/`-scanner + generated registry are gone
  from the workspace: derive-generated execution steps + explicit
  `expose_server!`/`expose_client!` declaration replace them — see the Exposure
  section above.
- **`examples/schema-smoke`** — end-to-end M1 proof: `#[wavedb]` derive output
  (`STRUCT_HASH`, roundtrip, shape consts, generated Pivot types) exercised
  directly — no `build.rs`, no `include!`. (Real example; `todo-app` still
  needs M4 `#[server]`/`Db`.)
- **`wavedb-storage` foundations** — `block` (`BlockDescriptor` u40·u20·u4 packing,
  `Run`, `BlockAllocator`: best-fit alloc / coalescing free / tail truncate) and
  `directory` (linear-hashing `bucket_index`/`next_split_bucket`, `Directory`).
  - **Page `hash_of` is SeaHash** — portable across arch/endianness, so journal replay
    rebuilds `data.bin` with the same routing even when the file moves machines. Random
    per-DB seed keeps DoS resistance.
- **`wavedb-storage` engine (M2 durable single-node `Store`)** —
  - **`BlockFile`** — `data.bin` as block-addressed file: superblock in block 0
    (magic + format version + per-DB seed, reserved via `RESERVED_BLOCKS`),
    positioned `pread`/`pwrite` run I/O, grow/truncate, `fsync`.
  - **`SlotPage`** — homogeneous record page: `crc32 + struct_hash + total_len +
id-list + blob`, crc-verified, reads correctly from a zero-padded run.
  - **`directory` page I/O** — `read_page`/`upsert_record`/`remove_record` and
    `split_next` (the page-moving half of linear hashing: repartition by the next
    hash bit, crash-safe descriptor reorder).
  - **`Journal`** — append-only WAL of `Write` batches; `fsync` on append =
    durability point; torn-tail-tolerant replay (truncates a half-written frame).
  - **`PageStore`** — implements core `Store` (`get`/atomic `apply`): journal-first
    → in-memory `BTreeMap` cache → inline settle into per-`STRUCT_HASH` pages, with
    split-on-growth. `open` rebuilds cache + pages + allocator by journal replay.
    `StorageError`→`Error::Backend` bridge added to core.
  - **core `BpTree`** (moved from storage's `PageBpTree`; the `BpTree` _trait_
    was deleted — one concrete `Store`-generic type in `wavedb_core::index`
    carrying `tenant`, shared by `PageStore` and the future IndexedDB store).
    Keys by the record's unique 10-byte `LocalId` (order = `CREATED_AT`).
    Insert with full leaf/internal split + cascade + root growth; idempotent;
    `search` streams record `Id`s by a `CREATED_AT` `Bound` **with descent
    pruning**; `remove` with **merge / redistribute / root-collapse**
    (underfull = <¼ cap; merge when the pair fits ¾ cap), all invariants
    checked by a test harness. Nodes encode via `WaveWire` behind a reserved
    page-kind tag and settle as ordinary `PageStore` values.
  - **Checked wire framing** — the WaveWire rule is fully
    in effect: `Write` derives `WaveWire` and journal frames are
    `[len][to_wire_checked(Vec<Write>)]`; the superblock body is
    `[magic][to_wire_checked(SuperblockBody)]` (version + seed inside the crc);
    and `SlotPage` is `[len][to_wire_checked(PageBody)]` (`struct_hash` +
    `(id, bytes)` entries — the hand-rolled header/id-list/offset format is
    gone). No engine structure hand-rolls its byte layout anymore; the only
    raw prefixes are the superblock magic and the `u32` payload length that
    delimits a page in a zero-padded run / a frame in the log.
  - **Hygiene** — 350-line-per-file budget enforced by
    `scripts/check_file_length.sh` (CI step); `maybe_split` checks only the
    touched bucket (O(1)); `wavedb-build` removed from the workspace.
  - **Per-`STRUCT_HASH` dictionaries + zstd page compression** — raw-content
    (no trainer) capped append-only sample buffer per type (`dictionary`
    module); **version = prefix length** (append-only ⇒ every old state is a
    prefix of the live buffer — old pages stay readable with no recompression
    or superseded-run bookkeeping); persisted in `data.bin` as its own block
    run via the shared allocator, rebuilt + re-persisted by journal replay.
    Page bodies store as a `PagePayload` enum: `Zstd { dict_len, raw_len,
bytes }` or `Raw` — per-type opt-out (`Directory::with_compression`;
    `PageStore` disables zstd for hot `BpTree` node pages) plus automatic
    `Raw` fallback when zstd cannot shrink a body. `directory` split into
    container/math + `directory_pages` (page I/O) for the file budget.
- **Per-type compile-time storage (`StructStorage`)** — the engine's runtime
  `HashMap<STRUCT_HASH, Directory>` + store-wide mutex are gone:
  - `#[wavedb]` emits (native only, `#[cfg(not(target_arch = "wasm32"))]`) one
    `static wavedb_storage::StructStorage` per declared type **and** per
    generated `{Name}Pivot` — the type's own cache (`RwLock<BTreeMap>`) and
    `Directory` slot (`Mutex<Option<…>>`), reached as `T::struct_storage()` /
    `T::storage_mem_cache()` / `T::storage_directory()`; schema crates gain a
    target-gated `wavedb-storage` dep (wasm expansion omits the slots).
  - `PageStore::open(dir, &[&'static StructStorage])` takes the slots as an
    **explicit registry** (`T::storage_entries()` = record + Pivot slots; the
    reserved BpTree-node slot auto-registers, compression off) — sorted-slice
    binary search, allowlist semantics: an unlisted hash is refused
    (`UnregisteredStructHash`) _before_ journaling. One open store per process
    (`EngineBusy` otherwise) since the slots are process-global statics.
  - Locking split: journal `Mutex` (append + cache commit under it ⇒ cache
    order == journal order), allocator `Mutex` — **journal + allocator stay
    shared by design** (one log = total order, one block space) — and per-type
    locks for everything else: reads (`Store::get_of`, new trait method with a
    `get` fallback default; `Collection`/`BpTree` pass their compile-time
    hashes) touch only their own type's cache lock. Settle converges pages to
    the cache's current bytes (idempotent, order-independent projection).
  - **Compression state is in the slot too** (`DictState` = zstd policy +
    `Dictionary` + persisted-run descriptor, own `Mutex`;
    `T::storage_dictionary()`): `Directory` is pure addressing again (no
    dict/compress fields — page fns take `&/&mut DictState`), dictionary
    persistence lives with `DictState::warm`, and the policy is declared on
    the type — `#[wavedb(compress = false)]` (storage config, never folded
    into `STRUCT_HASH`; generated Pivot slots always compress).
- **Typed collection layer** — the developer-facing surface over the (internal)
  `BpTree`, in the exact target shape
  (`Todo::collection(pivot, tenant).insert(&store, &todo)`):
  - core **`Collection<T: NonUniqueStruct>`** — `create` / `insert` / `save` /
    `remove` / `get` / `search` / `all`. Each mutating op is **one atomic
    `Store::apply` batch** (record + touched B+tree nodes via the new `plan_*`
    planners + `Pivot` rewrite when a root moved). Records/pivots are enveloped
    `[STRUCT_HASH (8 B LE)][wire]`, decode-verified (`UnknownStructHash` on a
    foreign id). Record `Id`s minted `KEY = CREATED_AT` nanos, `FLAG = 0`,
    counter salt; `remove` moves `current` → `dead` keeping the bytes (history
    navigable). New core errors: `PivotMissing`, `RecordMissing`.
  - **trait seams** — `NonUniqueStruct { type Pivot }` (macro-implemented, so a
    `Unique` type can't be collection-driven at compile time); `Pivot` gained
    `const STRUCT_HASH` (own identity, hashed under a reserved `Pivot` shape
    discriminator) and `replace_roots()`.
  - **macro emission** — `#[wavedb(NonUnique)]` emits `collection(pivot_id,
tenant)` + `create_pivot(store, tenant)`; `#[wavedb]` (Unique) emits
    anchor `get(store, tenant)` / `save(store, tenant)` (save = upsert, no
    create). Proven end-to-end in `schema-smoke`
    (`derived_collection_flow_end_to_end`) and over the durable engine in
    storage's `nonunique_collection.rs` (insert/save/remove survive reopen).
- **`examples/todo-app` on the exposure architecture** — the last
  `build.rs`/`include!(registry)` remnant replaced with `expose_server!` /
  `expose_client!` declaration modules (functions-only allowlist; all structs
  storage-only — `Auth`, the username registry, `Profile`, `Todo` are never
  wire-addressable; `REGISTRY` now comes from `expose_server!`). Aspirational
  (workspace-excluded) but architecture-correct.
