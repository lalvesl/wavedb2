# showcase

One runnable pair of processes touring the whole WaveDB developer surface:
the schema-as-protocol crate, a node, and a client with a local
write-through cache, live watches, version history, and offline reads.

## What it demonstrates

| Surface | Where |
| --- | --- |
| `Unique` holder + nested collections (records own `PivotId`s) | `Workspace` → `Project` → `Task` |
| Secondary indexes + filtered reads as `#[server]` fns | `by_name` / `by_status`, `tasks_with_status` |
| Server-side bootstrap (`create_pivot` is not wire-reachable) | `open_workspace`, `add_project` |
| Declared exposure (the list IS the registry) + side features | `expose_server!` / `expose_client!` in `src/lib.rs` |
| Typed client calls + streamed collection walk | `Task::collection(..).insert/save/remove/all` |
| Live watches, multiplexed over ONE connection per identity | `db.watch_unique` / `db.watch_collection` |
| Watches over plain HTTP polls | `--poll` (`Db::watch_via_polling`) |
| Version chain: who/when per version, live vs archived | `Workspace::history(&db)` + `Succession` |
| Conflict-safe saves (`Error::Conflict` = re-read and retry) | `save_with_retry` in `examples/client.rs` |
| Write-through cache + offline reads (node-first semantics) | `--offline` |
| Node durability (journal replay across restarts) | kill + restart the node |

## Run it

Terminal 1 — the node (persistent data dir, fixed port `4780`):

```sh
cargo run -p showcase --example node
```

Terminal 2 — the guided tour:

```sh
cargo run -p showcase --example client
```

Variants:

```sh
cargo run -p showcase --example client -- --poll     # watches over HTTP polls
cargo run -p showcase --example client -- --offline  # after killing the node
```

The offline pass reads the workspace, projects, and tasks entirely from the
client's local cache (a real WaveDB engine under `<tmp>/wavedb-showcase-cache`)
and shows an offline write being refused — write-through keeps the cache
strictly behind the node.

Restart the node after a kill and run the client again: the journal replays
the node's state, `open_workspace`/`add_project` are idempotent, and the
mirrors reconverge.

## Notes

- The client signs its own access token against the fixed `DEMO_SECRET` —
  demo plumbing only. The todo-app example carries the real login flow
  (`#[server]` functions issuing and rotating token pairs).
- Everything here compiles under the `server-side`/`client-side` feature
  contract: a deployed client would depend on this crate with
  `default-features = false, features = ["client-side"]`, and the
  `#[server]` bodies would never be compiled into it.
