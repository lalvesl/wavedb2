# RFC 0038 — Argon2 & OAuth/OIDC credentials

- **Status:** Planned (low priority) — deferred auth seams
- **Crates:** application `#[server]` fns, `wavedb::auth`
- **Depends on:** [RFC 0026](0026-auth-tokens.md)

## Summary

Two deferred credential seams on top of the shipped token machinery: a proper
**Argon2** credential-at-rest object (the examples still hash with sha256), and
an **OAuth / OIDC** path for federated login.

## Motivation

The token layer is done and proven ([RFC 0026](0026-auth-tokens.md)) —
stateless HMAC access tokens, rotating refresh tokens, session revocation. What
is *not* production-grade is how a password becomes a credential: `examples/todo-app`
sha256s, which is fine for an e2e demo and wrong for real deployment. And many
apps will not own passwords at all — they federate to an external IdP.

## Why low priority (deferred)

These are *seams*, not blockers: the token flow they feed already works, so a
demo runs end to end without them. Argon2 is a drop-in at the credential-object
boundary; OAuth/OIDC is a new `#[server(public)]` flow that mints the same
`TokenPair`. Neither changes the core, so both can land when a real deployment
needs them.

## Design (target)

- **Argon2 credential object.** A stored, hashed credential using Argon2id with
  tuned parameters, replacing the sha256 placeholder in the example auth flow.
  Verification stays inside a `#[server(public)] login` body
  ([RFC 0016](0016-server-functions.md)); the record is a storage-only type
  (never wire-addressable, [RFC 0017](0017-exposure-registry-and-side-features.md)).
- **OAuth / OIDC.** A public server flow that validates an external provider's
  token, resolves/creates the user, and issues WaveDB's own `TokenPair`
  ([RFC 0026](0026-auth-tokens.md)) — so the rest of the stack is unchanged
  (identity is still the in-body WaveDB token, [RFC 0020](0020-net-transport-dumb-tunnel.md)).

## Reference

`docs/example_auth.md` documents the current (placeholder) auth flow this
replaces the hashing half of.
