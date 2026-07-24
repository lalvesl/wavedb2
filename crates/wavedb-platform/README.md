# wavedb-platform

The **native ⇄ browser seam** — the bottom of the dependency chain, below
even `wavedb-core`. Everything above it stays target-independent; this crate
owns the three facts that differ per platform, behind one API compiled two
ways (conditional compilation is the dispatch — no traits, no `dyn`):

| module | native | wasm32-unknown-unknown |
|--------|--------|------------------------|
| `time` | `SystemTime` | `js_sys::Date::now()` |
| `rand` | `RandomState` hasher keys (OS entropy, no rand dep) | `window.crypto.getRandomValues` |
| `http` | hand-rolled HTTP/1.1 POST over a fresh `TcpStream` | `fetch` + `Request`, response body via `ReadableStreamDefaultReader` |

Why it exists: on wasm32-unknown-unknown `SystemTime::now()` **panics at
runtime**, there is no socket, and OS entropy is unreachable — while the
browser already ships all three as web APIs. Without this seam every crate
above would carry its own `#[cfg(target_arch = "wasm32")]` forks.

Notes:

- `http` is only the **client half** of the dumb tunnel: `post(addr, body)`
  → `Body::chunk()` streaming raw bytes in arrival order. Chunk boundaries
  carry no meaning; the `[len u32 LE][bytes]` framing on top is
  `wavedb-net::frames`. The server half stays in `wavedb-net::http` —
  a node is never a browser.
- `time::unix_nanos()` has millisecond precision in the browser; same-instant
  id mints stay distinct via the caller's salt counter, exactly as
  same-nanosecond native mints do.
- Errors are typed per layer as everywhere: `platform::Error` converts into
  `wavedb_net::Error::Platform` at the net seam.
