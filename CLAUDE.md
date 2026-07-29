# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Docs describe the TARGET, not the code

This is a clean rebuild of WaveDB. Every README (root and per-crate) describes the
**target** architecture; much of it is not built. Before assuming a crate, API, or
mechanism exists:

1. Check `Cargo.toml` workspace `members` vs `exclude` — excluded crates do not build.
2. Check `todo.md` (remaining work + DOING) and `todo_done.md` (what actually landed).

When a milestone lands, update `todo.md`'s DOING/DONE. READMEs carry `> Status:` blocks
where a documented mechanism isn't built yet — keep that honesty when editing docs.

## Commands

Development runs inside the Nix dev shell (`nix develop`); CI does the same. If already
in the shell (direnv), plain `cargo` works. The pre-commit bar:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets   # zero warnings (pedantic + nursery are warn = live)
cargo test --workspace                   # all green
scripts/check_file_length.sh             # 350 non-test lines per .rs file
```

Other commands:

```sh
cargo test -p wavedb-core                          # one crate
cargo test -p wavedb-storage --test nonunique_collection   # one integration test file
cargo test -p wavedb-core collection::             # filter by test name/path
cargo doc --workspace --no-deps                    # doc build (CI-gated)
cargo deny check                                   # license/advisory audit (CI-gated)
cargo nextest run --workspace --tests --release    # CI e2e job
nix build .#wasm                                   # size-optimised wasm artifact
# browser-only tests (IndexedDB store), from crates/wavedb-wasm — needs the
# nixpkgs chromedriver (the one wasm-pack downloads can't run on NixOS):
nix shell nixpkgs#chromedriver --command bash -c \
  'CHROMEDRIVER=$(which chromedriver) wasm-pack test --headless --chrome'
scripts/browser_demo.sh    # M5 exit: live node + browser demo (starts the
                           # contact-book example node, handles chromedriver)
scripts/registry_size.sh   # per-struct wasm cost of the exposure match
                           # (examples/registry-size at widths 1/16/64)
```

Toolchain is pinned by `rust-toolchain.toml` (1.96.0, edition 2024, includes
wasm32-unknown-unknown). Never build against anything else.

## Hard rules (breaking one is an architecture change)

Full rationale in `docs/development_standards.md`. The load-bearing ones:

- **No `dyn`, no sum-type registries.** All dispatch is a generated `match` on the
  64-bit `STRUCT_HASH` to concrete, monomorphized arms. This applies to macro
  expansions too — no fn-pointer tables, no runtime registration.
- **No serde.** Byte layouts are the `WaveWire` codec (`docs/wire_format.md`):
  `[STACK fixed-size][HEAP variable]`, little-endian, `usize`/`isize` never encodable.
- **`seahash` is pinned `=4.1.0`** — STRUCT_HASH identity is load-bearing; never loosen.
- **No format versioning pre-release (policy).** `FORMAT_VERSION` pinned at 1; on-disk
  layouts change freely between commits with no bump, no migration notes. An old
  `data.bin` is simply unsupported.
- **Errors are typed per layer** (`wavedb_wire::Error`, `wavedb_core::Error`,
  `StorageError`, net/node/client errors). Never fabricate a foreign layer's error
  inline — convert at the documented seam (`StorageError` → `core::Error::Backend`,
  core → `NodeError::from_core`, etc.). No `unwrap`/`expect`/`panic!` in library paths.
- **File budget: 350 non-test lines per `.rs`** (colocated `#[cfg(test)]` doesn't
  count). Over budget ⇒ split by layer. Complexity thresholds live in `clippy.toml`
  only (single source of truth, ratchet down only) — don't repeat numbers in docs.
- **`async` end to end**; the engine's futures are deliberately non-`Send`
  (current-thread `LocalSet` model) — `#![allow(clippy::future_not_send)]` at crate
  root is the established stance in core/storage/quick-node.

## Architecture (bottom-up)

Dependency chain: {`wire`, `platform`} → `core` → {`macros`, `storage`} → `net` →
`quick-node` → `wavedb` → `wavedb-wasm`.
The schema crate compiles into client and node — the schema IS the protocol; there is
no DTO layer and no query DSL (filtered reads = `#[server]` functions).

- **wavedb-platform** — the native ⇄ browser seam, cfg-switched (no traits): `time`
  (`SystemTime` / `Date.now()` — `SystemTime::now()` **panics** on wasm32; + `sleep`),
  `rand` (`RandomState` keys / `window.crypto`), `http` (the tunnel's **client half**:
  hand-rolled TcpStream POST / `fetch` + streamed body), `ws` (the WebSocket client
  half: hand-rolled RFC 6455 / browser `WebSocket`; `Conn::split()` for reader-task
  patterns), and `task` (`spawn_detached` = dedicated thread w/ current-thread
  runtime + LocalSet / `wasm_bindgen_futures::spawn_local` — **no tokio in wasm**).
  Everything above must route clock/entropy/client-HTTP through it — never name
  `SystemTime` or a socket directly.
- **wavedb-wire / wavedb-wire-derive** — standalone `WaveWire` codec (no STRUCT_HASH,
  no engine coupling) + derive. Gotcha: `#[derive(WaveWire)]` emits absolute
  `::wavedb_wire::` paths, so any crate using it needs `wavedb-wire` as a direct dep.
  Feature `validation` adds `to_wire_checked`/`from_wire_checked` (`[crc32][wire]`) —
  every disk/transport boundary uses it; no structure hand-rolls a byte layout.
- **wavedb-core** — `Id` (`KEY u64 · TENANT u48 · FLAG 1 · SALT 15`), `LocalId`
  (80-bit, tenant-stripped), `U48`, `Metadata` (version chain + `pivot_id` back-link +
  permission), the `Store` trait (`get`/`get_of`/atomic `apply`), the `Store`-generic
  `BpTree<K: NodeKey>` index, `Collection<T>` (the developer surface over the tree),
  `record.rs` (envelopes, id minting, `plan_chained_save`), `Overlay` (batch-pending
  read view so multiple plans on one tree compose into one atomic batch), and the
  `expose` module (`Command`/`Reply`/`Exposure` — the registry contract).
- **wavedb-macros** — `#[wavedb]` computes `STRUCT_HASH` (SeaHash over
  name+shape+fields — any schema change = new type), emits `WaveWire`, generated
  `{Name}Pivot`/`{Name}PivotId`, per-command exec steps `__wavedb_{get,save,insert,update,remove,all}`,
  per-type `static StructStorage` slots (native only; wasm expansion omits them), and
  secondary-index hooks from `#[wavedb::pivot(field)]`. `#[server]` emits a fn-type
  (own STRUCT_HASH + dispatch), the body retyped onto `ServerDb`, and a client stub.
  `expose_server!`/`expose_client!` are the **declared allowlist registry**: one match
  per operation over exactly the listed items; unlisted/excluded/wrong-shape all refuse
  as uniform `UnknownStructHash` (deliberately indistinguishable — security).
  **Side features (no-leak contract)**: a crate expanding these macros declares
  features named exactly `server-side`/`client-side`; body + dispatch +
  `expose_server!` compile only under `server-side`, stubs + `expose_client!` only
  under `client-side` (fn-type/STRUCT_HASH always). Deployed binaries pull the schema
  `default-features = false` + their side — the other side is never *compiled* in
  (the cfg is the guarantee, not LTO). Server-only helpers outside `#[server]` bodies
  carry the cfg by hand; wasm32 + `server-side` is a `compile_error!`. Defaults keep
  both on so schema-crate tests run the full loop.
- **wavedb-storage** — the node engine behind `Store`: `data.bin` (4 KiB blocks,
  superblock in block 0), per-STRUCT_HASH linear-hashed page directories, `SlotPage`
  (checked-wire envelope, per-type zstd dictionaries with version = prefix length),
  journal-first WAL (append + cache commit under the journal lock = the atomic unit),
  replay on open. Per-type state is compile-time (`StructStorage` statics) —
  consequence: **one open `PageStore` per process** (`EngineBusy`); tests serialize
  via an `engine_gate()` mutex and integration tests use a single `#[tokio::test]`.
- **wavedb-net** — hand-rolled minimal HTTP/1.1 POST as a **dumb tunnel**: no headers,
  cookies, or status semantics as API; the body is a self-contained
  `Request { tenant, CommandFrame { struct_hash, command, payload } }` and a WaveDB
  refusal is a 200 carrying `NodeError`. Functions and structs share one hash space —
  a fn call is indistinguishable from an object op at the frame level. `NetClient` +
  `frames::FrameReader` are target-independent (POST/body via `wavedb-platform`);
  only the server half (`net::http`) is native-gated. **Every exchange routes through
  `net::manager`** — one never-ending background task per process (M7, user-directed):
  it runs all POSTs, multiplexes all watches of one `(addr, identity)` over ONE
  WebSocket connection (`Hello` once, per-topic subscribe, fan-out; lifecycle owned by
  the manager loop), and can watch over plain HTTP instead ("anything new?" polls —
  `net::sync`, reserved hash `"WDB.SYNC"` routed before the registry; node buffers per
  token-session in `quick-node::poll`, replace-semantics topic declaration).
- **wavedb-quick-node** — library (no bin): `Server::new(REGISTRY).data_dir(d).serve(addr)`.
  `expose_server!` also emits `StorageRegistry`, so `.registry(REGISTRY)` alone opens
  the `PageStore`. Gates 4–6 (permission/validate/preprocess) are an M8 seam.
- **wavedb (client)** — `Db::connect(addr, user, tenant)` (transport-only) and the
  M6 `Db::open(CLIENT_REGISTRY, addr, user, tenant, app)` family attaching the local
  **write-through cache** — WaveDB caching WaveDB, cfg-switched like the platform
  seam: native = `PageStore` under `~/.cache|XDG_CACHE_HOME|%LOCALAPPDATA%`
  `/<app>` (the app is the leaf directly under the base, XDG-style, no shared
  `wavedb/` parent; auto-created; `open_at` for an explicit dir), wasm =
  `wavedb::cache::IdbStore` (`wavedb-wasm` only re-exports it). Semantics
  (`client_cache.rs`): **node-first** — acknowledged ops mirror best-effort under
  node-minted ids (`Collection::adopt`; `All` frames carry `(Id, T)` for this);
  reads fall back only on `Error::Transport` and only when the cache holds the
  answer (absence propagates the fault; refusals never fall back); offline writes
  refuse (queueing is M7). `db.local()` is the cache's direct `LocalHandle`. One
  engine per process ⇒ a `Db::open` client and a node never share one (tests run
  the node as a child process — see `local_cache_e2e.rs`). Typed calls spell
  `T::get(&db)` / `v.save(&db)` / `T::collection(pivot)` via the `DbHandle` seam;
  `ServerDb` mirrors it node-side for `#[server]` bodies. Live sync (M7):
  `db.watch_unique::<T>()` / `db.watch_collection::<T>(pivot)` (token required)
  yield typed `WatchEvent`s and mirror each into the cache before yielding; watches
  multiplex through `net::manager` (one WS connection per identity), or poll over
  HTTP with `db.watch_via_polling(interval)`.

## Data-model invariants

- `save` is an upsert — there is no `create`. A save archives the old version and
  chains it through `Metadata` (`old_modification_id`/`new_modification_id`); bytes
  are never destroyed. Only `remove` writes the `dead` tree.
- NonUnique record identity `Id` is minted at `insert` and never changes; `save`
  re-keys only the secondary indexes whose fields changed (primary key is the
  immutable `CREATED_AT`).
- Every mutating collection op is exactly **one atomic `Store::apply` batch**
  (record + touched B+tree nodes + Pivot rewrite when a root moves).
- Stored values are STRUCT_HASH-headed: user records
  `[STRUCT_HASH][meta_len][Metadata][body]`; Pivots `[STRUCT_HASH][wire]`; BpTree
  nodes `[BPTREE_NODE_HASH][kind u8][wire]`. Decode verifies the head.
- Pivot instances are created explicitly (`create_pivot`), one per tenant per type;
  the holder stores the `PivotId`. The Pivot is rewritten only when a root moves.

## Testing conventions

- Unit tests colocate in `#[cfg(test)] mod tests`; cross-module behaviour in `tests/`.
- Codecs get roundtrip + failure cases asserting the **specific** error variant.
- Storage changes need a durability angle (reopen-and-replay or kill-during-write).
- Test names state behaviour (`tampered_payload_is_crc_mismatch`), not the method.
- Anything touching `PageStore` must respect the one-store-per-process rule (see
  `engine_gate()` in the page_store tests, and the single-test pattern in
  `crates/wavedb-quick-node/tests/node_http.rs`). That includes `Db::open`
  clients: a cache-and-node test needs two processes — re-exec the test binary
  as the node (`examples/contact-book/tests/local_cache_e2e.rs`).

## Commits

Conventional commits (`feat:`/`fix:`/`docs:`/`refactor:`/`perf:`/`test:`), imperative
subject, body explains why when the diff doesn't.
