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
  (`SystemTime` / `Date.now()` — `SystemTime::now()` **panics** on wasm32), `rand`
  (`RandomState` keys / `window.crypto`), `http` (the tunnel's **client half**:
  hand-rolled TcpStream POST / `fetch` + streamed body). Everything above must route
  clock/entropy/client-HTTP through it — never name `SystemTime` or a socket directly.
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
  only the server half (`net::http`) is native-gated.
- **wavedb-quick-node** — library (no bin): `Server::new(REGISTRY).data_dir(d).serve(addr)`.
  `expose_server!` also emits `StorageRegistry`, so `.registry(REGISTRY)` alone opens
  the `PageStore`. Gates 4–6 (permission/validate/preprocess) are an M8 seam.
- **wavedb (client)** — `Db::connect(addr, user, tenant)`; typed surface lives **on
  `Db`** (`db.get::<T>()`, `db.save(&v)`, `db.collection::<T>(pivot)`), not the
  documented `T::get(&db)` — the macro's inherent `T::get(store, tenant)` wins method
  resolution (known collision; unification is planned work, see `todo.md`).
  `ServerDb` mirrors this surface node-side for `#[server]` bodies.

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
  `crates/wavedb-quick-node/tests/node_http.rs`).

## Commits

Conventional commits (`feat:`/`fix:`/`docs:`/`refactor:`/`perf:`/`test:`), imperative
subject, body explains why when the diff doesn't.
