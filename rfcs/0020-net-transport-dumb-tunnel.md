# RFC 0020 — The net transport (dumb tunnel)

- **Status:** Implemented
- **Crates:** `wavedb-net`
- **Code:** `frame.rs`, `frames.rs`, `http.rs`, `client.rs`

## Summary

The transport is a **dumb tunnel**: hand-rolled minimal HTTP/1.1 POST with no
headers, cookies, or status semantics as API. The POST body is a self-contained
`Request { tenant, CommandFrame { struct_hash, command, payload } }`, and a
WaveDB refusal is a **200 carrying `NodeError`** — never an HTTP status. Functions
and structs share one hash space, so at the frame level a function call is
indistinguishable from an object op.

## Motivation

WaveDB carries its *own* identity, framing, and error model
([RFC 0026](0026-auth-tokens.md), [RFC 0010](0010-metadata-and-record-envelopes.md),
[RFC 0002 §5](0002-architectural-hard-rules.md)). Layering that on HTTP semantics
(status codes as errors, headers as identity, cookies as sessions) would mean two
overlapping models and a CORS/credentials surface. Treating HTTP as a *byte pipe*
— POST bytes in, get bytes back — removes all of it: the body is the whole
protocol.

## Design

- **`Request` = `{ tenant/auth, CommandFrame }`**, itself a `WaveWire` struct.
  `CommandFrame = { struct_hash, command, payload }` — one uniform frame for
  object ops **and** `#[server]` calls ([RFC 0016](0016-server-functions.md)); the
  function arm ignores `command` and decodes `payload` as the args tuple.
- **`command`** is `Get`/`Save` (Unique) or `Insert`/`Update`/`Remove`/`All`/
  `Changes` (NonUnique / sync).
- **Refusal is in-body.** A `NodeError { code, struct_hash, field, message }`
  rides the reply envelope as a 200; the client maps it to the typed `Error`.
  The uniform `UnknownStructHash` ([RFC 0017](0017-exposure-registry-and-side-features.md))
  is one such code.
- **Framed streams.** A response is a sequence of length-prefixed frames
  (`[len u32 LE][StreamFrame]`; `Item(bytes)* End(Response)`) written
  progressively into one POST body — no `content-length`, `connection: close`
  delimits. `NetClient::call` (scalar) / `call_stream` (items as flushed);
  `frames::FrameReader` reassembles on both targets.
- **Target split.** `NetClient` + `FrameReader` are target-independent (POST/body
  via the [platform seam](0006-platform-seam.md)); only the **server** half
  (`net::http`) is native-gated.
- **No preflight.** The wasm `post` sends no `content-type` (a CORS *simple*
  request), and node heads carry `access-control-allow-origin: *` — not a
  boundary, since identity is the in-body token, never ambient credentials
  ([RFC 0025](0025-wasm-indexeddb-target.md)).

## The WebSocket half (same dumb-tunnel stance)

Push notifications and live watches need a connection the node can write down,
so the transport also speaks **hand-rolled RFC 6455** — same stance as the POST
tunnel: binary messages only as API, no extensions, no subprotocols. `sha1` (the
RustCrypto family already present) is the one new dep, for the accept key;
base64 is a ~20-line encode-only helper.

- **Shares the POST port.** The head parser routes `GET` + `Upgrade` alongside
  POST (`read_request → Incoming::{Post, Upgrade{key}}`), so no second listener.
- **Identity once.** The token is presented **once**, in `ClientMsg::Hello`, and
  every later message executes as that verified caller — the connection *is* the
  identity binding an HTTP POST can never be ([RFC 0026](0026-auth-tokens.md)).
- **Envelopes.** `ClientMsg { Hello | Call | Subscribe | Unsubscribe }`,
  `ServerMsg { HelloOk | Item | End | Event | TopicOk }`. `Call` runs one
  `CommandFrame` through the same gates/dispatch as a POST (FIFO per connection).
  `TopicOk` acks a subscribe/unsubscribe **FIFO**, so a returned watch cannot
  miss a later mutation; an anonymous `Subscribe` closes the connection (loud
  refusal). `Event` carries a `RecordEvent` ([RFC 0022](0022-live-sync-navigation-catchup.md)).
- **Same surface both targets.** `wavedb_platform::ws` is the native RFC 6455
  codec/handshake vs the browser `WebSocket` bridge; `Conn::split()` gives the
  reader-task pattern the [connection manager](0021-connection-manager.md) and the
  node session loop ([RFC 0023](0023-quick-node-and-gates.md)) both use.

The Phase-11 `tokio-tungstenite` / `gloo-net` / `axum` declarations stay unused,
exactly as `reqwest` did once the POST tunnel was hand-rolled.

## Consequences

- **One hash space, one frame** is what lets `#[server]` functions reuse the
  entire transport unchanged, and what makes probing the schema impossible
  (a refusal reveals nothing).
- **Every exchange routes through the manager** — the actual dial/pool/teardown
  lives in [RFC 0021](0021-connection-manager.md), not in `NetClient` directly.

## Alternatives

- **REST-ish semantics (status codes, headers, cookies)** — rejected for the
  two-models / CORS-credentials cost above; the dumb tunnel is a byte pipe.
