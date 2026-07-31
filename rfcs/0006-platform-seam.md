# RFC 0006 — The platform seam (native ⇄ browser)

- **Status:** Implemented
- **Crates:** `wavedb-platform` (the bottom crate, below `core`)
- **Code:** `crates/wavedb-platform/src/{time,rand,http,ws,task}.rs`

## Summary

`wavedb-platform` owns every fact that differs between native and
`wasm32-unknown-unknown`, behind **one API compiled two ways** — `cfg`-switched,
**no traits**. Everything above it (core, net, client) routes clock, entropy,
client-HTTP, WebSocket, and task-spawning through it and never names a
`SystemTime` or a socket directly. This is the single seam that lets the same
source compile to Android/Windows/Linux/macOS/iOS and the browser.

## Motivation

The wasm target is not "native with a different allocator" — key primitives are
absent or lethal:

- **`SystemTime::now()` *panics* on wasm32.** Any id minting or token clock that
  named it directly would crash in the browser.
- **No tokio in wasm** (a hard user constraint, for binary size): the runtime
  model is entirely different (`wasm_bindgen_futures`, single-threaded).
- **Entropy, HTTP, and WebSocket** come from `std`/sockets natively but from
  `window.crypto` / `fetch` / the browser `WebSocket` object in wasm.

Pushing these behind a trait would add `dyn` (forbidden,
[RFC 0002 §1](0002-architectural-hard-rules.md)) or a generic parameter through
the whole stack. A `cfg`-switched module with one signature, two bodies, keeps
the call sites identical and the wrong target's code out of the binary entirely.

## Design — the five facts

- **`time`** — `SystemTime` vs `Date.now()`; `key_nanos()` (the shared minting
  formula, [RFC 0005](0005-composite-ids-and-bit-budgets.md)); `sleep`.
- **`rand`** — `RandomState` keys vs `window.crypto.getRandomValues`
  (quick-node's default node secret draws from it).
- **`http`** — the tunnel's **client half**: hand-rolled `TcpStream` POST vs
  `fetch` + a streamed response body. (The *server* half stays native — see
  [RFC 0020](0020-net-transport-dumb-tunnel.md).)
- **`ws`** — the WebSocket client half: hand-rolled RFC 6455 vs the browser
  `WebSocket`; `Conn::split()` for reader-task patterns
  ([RFC 0020](0020-net-transport-dumb-tunnel.md)).
- **`task`** — `spawn_detached` (a dedicated thread with a current-thread
  runtime + `LocalSet`) vs `wasm_bindgen_futures::spawn_local`. This is what the
  connection manager runs on ([RFC 0021](0021-connection-manager.md)).

## Consequences

- tokio stays strictly behind `cfg(not(target_arch = "wasm32"))`; the wasm build
  runs on `wasm_bindgen_futures`.
- The same seam pattern (cfg-switch, not a trait) recurs for the **client
  cache** backend (`PageStore` vs `IdbStore`,
  [RFC 0024](0024-client-db-and-cache.md)) — the platform seam is the template.

## Alternatives

- **A `Platform` trait** threaded generically: rejected for the `dyn`/generic
  cost above and because a compile-time `cfg` split gives a *stronger* guarantee
  — the other target's code is not merely unreachable, it is never compiled.
