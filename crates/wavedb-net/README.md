# wavedb-net

The network transport layer. **WaveDB _is_ the wire protocol** — there is no
separate REST/RPC layer, no DTO split, no API schema to keep in sync with
storage. Whatever the client serializes — a CRUD request or a server-function
call — the server deserializes straight into the engine. Both record operations
(`get`/`save`/`insert`/`remove`/collection walk) and **server-function calls**
ride the same `Transport` as **one uniform `CommandFrame`** (`struct_hash` +
`command` + `payload`) — functions and structs share the hash space, so there is
no separate call frame (see [Command envelope](#command-envelope--dispatch)).
Collection reads (`all` / `by_field`) and collection-returning server fns are
**async iterators**: the node streams record items back as a sequence of frames
rather than buffering a whole `Vec`, so the client can stop early.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module    | Responsibility                                                  |
| --------- | --------------------------------------------------------------- |
| `frame`   | Wire frame encode/decode.                                       |
| `ws`      | WebSocket transport (preferred).                                |
| `http`    | HTTP POST transport (fallback) + the client queue.              |
| `notify`  | "object changed" notifications (push / piggyback).              |
| `browse`  | Live-browse / screen-sync surface.                              |
| `auth`    | Session + node-to-node HMAC tokens, login, identity derivation. |
| `request` | Request/response envelopes (`TransportResponse`, `NodeError`).  |
| `metrics` | Per-node transport metrics.                                     |
| `mock`    | In-process transport for tests.                                 |

The crate provides a `Transport` trait with concrete WebSocket / HTTP / mock
implementations; the same operations run on servers, native clients, and (via
`wavedb-wasm`) browsers.

---

## Transports

> **Status: HTTP POST only for now.** The current rebuild wires a single
> transport — **HTTP POST** — on both native and browser. WebSocket (and with it
> server push, idle-tick piggyback, and Bloom screen-sync) is **deferred**; the
> table below is the target shape. The request path is built so the transport is
> swappable later without touching the command / dispatch / auth layers.

| Transport         | Native | Browser | Status    | Notes                                                  |
| ----------------- | ------ | ------- | --------- | ------------------------------------------------------ |
| **HTTP POST**     | ✓      | ✓       | **wired** | The only transport for now. FIFO queue per client.     |
| **WebSocket**     | pref.  | pref.   | deferred  | Bidirectional, push-capable; carries Bloom sync.       |
| **Future native** | plan.  | n/a     | planned   | Higher-throughput native-only transport, in scoping.   |

### HTTP POST: single-queue with piggybacked notifications

Plain HTTP can't push, so the client runs a small queue:

1. **One FIFO queue per client** — no concurrent in-flight POST per session;
   ordering is deterministic and the server can attach state changes to the next
   response.
2. **Responses carry more than was asked for** — notifications about on-screen
   objects that changed ride along on the next response (same `new state` event
   the WebSocket pushes).
3. **Idle ticks** — when the queue empties, the client sends empty POSTs at
   `http_poll_interval` so the server can flush pending notifications, backing
   off when none arrive.
4. WebSocket and the future native transport skip all of this — they push
   directly and run the request queue with normal concurrency.

The application code is identical across transports: it always reacts to "object
changed" events; the transport decides push vs. piggyback.

---

## Command envelope & dispatch

**The transport is a dumb tunnel — WaveDB uses no HTTP-protocol features.** No
`Authorization` header, no cookies, no HTTP status semantics: identity, the
command, and errors all ride **inside** the WaveDB wire object. An HTTP POST body
*is* a complete, self-contained `Request`; HTTP only moves the bytes.

Every operation — a record op **or** a server-function call — rides the transport
as **one uniform frame**; there is no separate request type for functions:

```
Request {
    auth:  AccessToken,   // identity — carried IN the body, never an HTTP header
    frame: CommandFrame,  // record op OR server-fn call — same shape for both
}

CommandFrame {
    struct_hash: u64,     // a #[wavedb] struct OR a #[server] fn — ONE hash space
    command:     Command, // struct op; ignored for a function (the hash IS the op)
    payload:     Wire,    // record / Id (struct op) or the args tuple (server fn)
}
```

**From the frame alone you cannot tell a server-fn call from an object save** —
and you don't need to. The sole discriminator is the generated **`match
struct_hash`**: every hash is, at compile time, either a `#[wavedb]` struct or a
`#[server]` function, and the matched arm knows which. A struct arm reads
`command` and treats `payload` as the record or `Id`; a function arm ignores
`command` (its hash names exactly one function) and decodes `payload` as the args
tuple, then runs the body. Same frame, same match — the builder never
special-cases a "call" frame.

Over **HTTP POST** (the only wired transport) every POST body is a full `Request`
— the access token is re-sent inline each time, because plain HTTP has no
connection to bind it to. Over **WebSocket** (deferred) the token is presented
**once at the handshake** and the socket is bound to that identity, so subsequent
frames carry only the `CommandFrame`.

The client builds the frame from a typed call (`record.save(&db)`,
`collection.insert(&db, …)`) right after the local write-through. For a struct,
`command` is the shape's operation set:

| Shape         | `Command` values                                     |
| ------------- | ---------------------------------------------------- |
| **Unique**    | `Get`, `Save`                                        |
| **NonUnique** | `Insert`, `Update`, `Remove` (+ `Get` / `All` reads) |

> The NonUnique update command is `Update`; the client-side method stays
> `record.save(&db)` (save = upsert). One name on the wire, the familiar `save`
> at the call site.

The node routes the frame through the generated registry — **one `match` on
`struct_hash`** to the concrete type's arm. For a struct hash the arm runs a
shape-specific `match command { … }` to that type's compile-time `get`/`save` /
`insert`/`update`/`remove` fn, which drives the storage engine (allocator,
journal, pages, `Pivot`/`BpTree`); for a function hash the same match runs the
server-fn body on the decoded args instead. Identity and permission are enforced
**before** the engine runs — see
[`wavedb-quick-node` § node-side enforcement](../wavedb-quick-node/README.md#node-side-enforcement).

---

## Bloom Filter Screen-Sync

> **Deferred** — screen-sync rides WebSocket push, which the current rebuild does
> not wire (HTTP POST only). The protocol below is the target.

State-sync for the **online** read path. Clients keep a Bloom filter of the
128-bit IDs currently on screen and send it over WebSocket. The owner node
compares it against its live records and pushes back **only the deltas** — new
objects, updated records, deletions. Event-driven: every accepted mutation
triggers a notification to subscribers whose filters might match. For clients
long offline (screen state far behind reality), sending the explicit array of
on-screen IDs back for revalidation is cheaper than the filter.

---

## Authentication

The request hot path is **stateless**: an access token is HMAC-signed with the
cluster key, so any node (owner or replica) verifies it locally — no session
store, no lookup per request. The "same binary, no extra infra" property holds.
Revocation is added by a **refresh token** that _is_ tracked, but it is consulted
only when minting a new access token (rare), never on ordinary requests.

### Access token (stateless, short-lived)

```
access = { user: U48, tenant: U48, expiry, purpose: Access } + HMAC(cluster_key, …)
```

The node **derives `user`/`tenant` from the verified token, never from any
unsigned field of the operation** — a client cannot claim an identity it wasn't
issued. The token rides **inside the WaveDB request envelope** (the POST body),
**not** an HTTP `Authorization` header — the transport stays a dumb tunnel. TTL is
**short** (minutes); over HTTP POST every request re-sends it, over WebSocket
(deferred) it is presented once at the handshake and bound to the connection.
Verification is signature + expiry, nothing else loaded.

### Refresh token (revocable)

Login also issues a long-lived **refresh token** bound to a stored session record:

```
refresh = { session_id, user, tenant, expiry, purpose: Refresh } + HMAC(cluster_key, …)
```

`session_id` points at a **session record** — a Unique-style `#[wavedb]` object
`{ user, tenant, issued, revoked }`. A `Refresh` request verifies the token's
HMAC **and** loads that record:

- record live & not expired → mint a fresh access token (TTL resets), and
  **rotate** the refresh token (new `session_id`/counter); a replayed old refresh
  token ⇒ theft signal → revoke the session;
- record `revoked` or missing → refuse.

**Revocation = mark the session record `revoked` (or delete it)** — one record
write. The next `Refresh` fails immediately, and any outstanding access token
dies on its own within one short TTL. So "log out this session / log out
everywhere, now" is cheap, and the per-request path stays store-free.

### Token families (the `purpose` tag)

One HMAC machine, distinct `purpose` tags so a token minted for one path can't be
replayed on another:

| Purpose      | Issued to | Carries                        | Used for                          | Stateful?          |
| ------------ | --------- | ------------------------------ | --------------------------------- | ------------------ |
| `Access`     | end users | `user`, `tenant`, short expiry | every client request after login  | no                 |
| `Refresh`    | end users | `session_id`, `user`, expiry   | minting new access tokens         | yes (record check) |
| node-to-node | nodes     | node id, expiry                | `Replicate` and future node paths | no                 |

### Login & credentials

Login is a **`#[server]` function** (runs on the node, see
[`wavedb-macros`](../wavedb-macros/README.md#server-functions--server)) that
validates a credential, creates the session record, and mints an **access +
refresh** pair. Two credential sources feed the **same** login path and mint the
**same** pair:

1. **Local** — a Unique `#[wavedb]` credential object per user (e.g. an Argon2
   password hash + linked-provider list), stored at its anchor like any other
   record. No separate auth database.
2. **External (OAuth/OIDC)** — Google et al.; the node verifies the provider's
   token, looks up / links the local user, then mints the session.

### Unauthenticated tier

A client with no token connects as `user = U48::MAX`. The session is restricted
to **login** and **public reads** (records whose `Metadata.permission` is
`Public`); everything else is refused. Permission enforcement itself is per
record (`PermissionRef`) — see
[`wavedb-core`](../wavedb-core/README.md#permissions).

### Server-function access policy

Calling a server function is itself gated:

- **`#[server]` (default) requires a logged-in session** — unreachable from the
  unauthenticated tier.
- **`#[server(public)]` is open to anyone**, including `user = U48::MAX`. This is
  how `login` (and `refresh`, which carries its own refresh token) is reachable
  before an access token exists.

The check lives **inside the generated function body, not in the registry
match**. The `#[server]` macro injects an auth guard at the top of the node-side
body (skipped for `public`); the `match struct_hash { … }` only routes the
frame's `struct_hash` to its body and carries no per-function auth policy. Keeping auth
out of the match keeps build-time dispatch uniform — one routing arm per
function, nothing else — and identity inside the body is the verified Access
token's `user`/`tenant`, never the request body.

> Worked end-to-end example (login → request → refresh → revoke):
> [`docs/example_auth.md`](../../docs/example_auth.md).
