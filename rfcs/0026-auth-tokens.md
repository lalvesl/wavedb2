# RFC 0026 — Auth: access & refresh tokens

- **Status:** Implemented (M8; landed 2026-07-10)
- **Crates:** `wavedb-net` (`auth`), `wavedb` (`auth`), application `#[server]` fns
- **Code:** `net::auth`; `wavedb::auth::{AuthSession, TokenPair}`; reference
  `docs/example_auth.md`

## Summary

Identity is a **stateless HMAC access token** carried *inside* the request body
(never an HTTP header) plus a **refresh token** bound to a stored session record.
The verified identity threads the whole stack as a `Caller { user, tenant }`, and
every `Metadata.user` is stamped from the token — so "who wrote this version"
([RFC 0010](0010-metadata-and-record-envelopes.md)) is authenticated, not
claimed.

## Motivation

The dumb tunnel carries no ambient credentials
([RFC 0020](0020-net-transport-dumb-tunnel.md)) — identity must be self-contained
in the body, verifiable by the node with no session lookup on the hot path, and
revocable when a token is stolen. A stateless access token (fast, no lookup) plus
a stateful refresh (revocable, replay-detecting) gives both.

## Design

- **Access token** — claims `{ user, tenant, expires_at, purpose, session,
  nonce }` + HMAC-SHA256, 15-min TTL, verified per request by **gate 1**
  ([RFC 0023](0023-quick-node-and-gates.md)). Rides in `Request.auth`
  (`Auth::Anonymous { tenant } | Auth::Token(bytes)`).
- **Refresh token** — bound to a `wavedb::auth::AuthSession` record, stored
  **hashed**. **Rotate on use**; a replayed refresh = theft signal → the session
  is revoked on the spot. Revocation is one record write (`issue_pair` /
  `refresh_pair` / `revoke` over any `DbHandle`).
- **Tiers.** `login` / `refresh` / `logout` are `#[server(public)]` fns
  ([RFC 0016](0016-server-functions.md)) returning `wavedb::TokenPair`. A plain
  `#[server]` fn refuses `user == U48::MAX` (the unauthenticated tier) before
  decoding; every *struct* command refuses it uniformly (`Unauthorized`).
- **Tenant binding is the isolation.** A caller only ever executes in the tenant
  its token names; a claimed tenant cannot override the token's. This *is* today's
  tenant isolation, standing in for the deferred per-record grant path
  ([RFC 0013](0013-permissions.md)).
- **Node secret** — `Server::secret([u8;32])` or a random one per boot, published
  process-wide (`node_secret`) for the minting helpers (one node per process,
  like the engine slots).

## Deferred

- **Argon2** credential object (examples still sha256) and the OAuth/OIDC path
  ([RFC 0038](0038-argon2-and-oauth-credentials-PLANNED-LOW.md)).
- **Per-record grants** (gate 4) — see [RFC 0013](0013-permissions.md).

## Proven

`examples/todo-app` e2e: a claimed tenant cannot override the token's; anonymous
non-public calls refused; a replayed refresh revokes the whole session; logout
kills the next refresh; expired / forged / wrong-purpose tokens refused.
