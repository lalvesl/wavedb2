# wavedb-net

The network transport layer. **WaveDB _is_ the wire protocol** — there is no
separate REST/RPC layer, no DTO split, no API schema to keep in sync with
storage. Whatever the client serializes — a CRUD request or a server-function
call — the server deserializes straight into the engine. Both record operations
(`get`/`save`/`delete`/collection walk) and **server-function calls**
(`CallServerFn { fn_hash, Wire args }`) ride the same `Transport`.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md).

## Module map

| Module    | Responsibility                                                 |
| --------- | -------------------------------------------------------------- |
| `frame`   | Wire frame encode/decode.                                      |
| `ws`      | WebSocket transport (preferred).                               |
| `http`    | HTTP POST transport (fallback) + the client queue.             |
| `notify`  | "object changed" notifications (push / piggyback).             |
| `browse`  | Live-browse / screen-sync surface.                             |
| `auth`    | Session + node-to-node HMAC tokens, login, identity derivation. |
| `request` | Request/response envelopes (`TransportResponse`, `NodeError`). |
| `metrics` | Per-node transport metrics.                                    |
| `mock`    | In-process transport for tests.                                |

The crate provides a `Transport` trait with concrete WebSocket / HTTP / mock
implementations; the same operations run on servers, native clients, and (via
`wavedb-wasm`) browsers.

---

## Transports

| Transport         | Native | Browser | Notes                                                  |
| ----------------- | ------ | ------- | ------------------------------------------------------ |
| **WebSocket**     | pref.  | pref.   | Bidirectional, push-capable; carries Bloom sync.       |
| **HTTP POST**     | back.  | back.   | When WebSocket is blocked (proxies, restrictive nets). |
| **Future native** | plan.  | n/a     | Higher-throughput native-only transport, in scoping.   |

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

## Bloom Filter Screen-Sync

State-sync for the **online** read path. Clients keep a Bloom filter of the
128-bit IDs currently on screen and send it over WebSocket. The owner node
compares it against its live records and pushes back **only the deltas** — new
objects, updated records, deletions. Event-driven: every accepted mutation
triggers a notification to subscribers whose filters might match. For clients
long offline (screen state far behind reality), sending the explicit array of
on-screen IDs back for revalidation is cheaper than the filter.

---

## Authentication

Auth is **stateless** — there is no session store. A token is HMAC-signed with
the cluster key, so any node (owner or replica) verifies it locally; this keeps
the "same binary, no extra infra" property.

### Session token

```
token = { user: U48, tenant: U48, expiry, purpose } + HMAC(cluster_key, …)
```

The node **derives `user`/`tenant` from the verified token, never from the
request body** — a client cannot claim an identity it wasn't issued. Expiry
bounds replay; `Drop`/logout just stops sending the token (stateless, so nothing
to revoke server-side beyond expiry — short TTLs + re-issue cover this).

### Token families (the `purpose` tag)

One HMAC machine, distinct `purpose` tags so a token minted for one path can't be
replayed on another:

| Purpose     | Issued to | Carries                    | Used for                          |
| ----------- | --------- | -------------------------- | --------------------------------- |
| `Session`   | end users | `user`, `tenant`, expiry   | every client request after login  |
| node-to-node | nodes    | node id, expiry            | `Replicate` and future node paths |

### Login & credentials

Login is a **`#[server]` function** (runs on the node, see
[`wavedb-macros`](../wavedb-macros/README.md#server-functions--server)) that
validates a credential and mints a `Session` token. Two credential sources feed
the **same** login path and mint the **same** token:

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
