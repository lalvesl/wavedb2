# RFC 0014 — Schema-evolution lookup hooks

- **Status:** Planned (the hook seam; `first_try` / `fallback_not_found`)
- **Crates:** `wavedb-core` (surface), application code (bodies)
- **Depends on:** [RFC 0004](0004-struct-hash-and-schema-evolution.md)

## Summary

`STRUCT_HASH` makes a changed struct a *new type*, so old and new bytes coexist
with no migration. *Bridging* them — reading a V1 record where the code now
wants V2 — is done by two optional **application hooks**, not an engine walk:

- **`first_try`** runs *before* a read hits storage — synthesise the value
  (e.g. decode the older `STRUCT_HASH` and map it forward);
- **`fallback_not_found`** runs *after* a miss — fetch or derive a default.

## Motivation

The alternative to migrations is coexistence ([RFC 0004](0004-struct-hash-and-schema-evolution.md)),
but coexistence still needs an answer to "the code asks for `AboutUser` V2 and
only a V1 record exists." A global upgrade walk is exactly the migration step
this design exists to avoid. Two small, *application-owned* hooks put the
bridging logic where the domain knowledge is, and only on the paths that need
it — no cost for types that never changed.

## Design (target)

- Both hooks are **per-type, application-supplied**, defaulting to nothing.
- `first_try` is a read *pre-empt*: if it returns a value, storage is never
  touched. This is where a V1→V2 shim lives (read the old hash, map fields).
- `fallback_not_found` is a read *post-miss*: last chance to synthesise a
  default or fetch from elsewhere before returning `None`.
- They compose with the client cache's node-first reads
  ([RFC 0024](0024-client-db-and-cache.md)) — the hook is a property of the typed
  read, above the transport.

## Status

The hooks are **documented target, not yet built** — recorded here so the idea
is not lost between "schema evolution by hash" (built) and the M9 developer-
experience milestone (the schema-evolution cookbook: the `first_try` /
`fallback_not_found` patterns). This RFC is the placeholder that keeps the
bridging story explicit until the seam lands.

## Alternatives

- **A global migration walk / version-upgrade chain** — the classic approach,
  rejected as the very friction [RFC 0004](0004-struct-hash-and-schema-evolution.md)
  removes.
