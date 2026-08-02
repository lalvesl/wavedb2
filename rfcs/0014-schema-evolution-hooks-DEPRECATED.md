# RFC 0014 — Schema-evolution lookup hooks

- **Status:** Deprecated
- **Superseded by:** [RFC 0040](0040-schema-migration-and-version-skew-DEPRECATED.md)
- **Crates:** `wavedb-core` (surface), application code (bodies)

## What it proposed

Two optional, per-type **application hooks** to bridge a `STRUCT_HASH` version
skew without an engine-wide migration walk: `first_try` (a read *pre-empt* — decode
the older hash and map it forward before storage is touched) and
`fallback_not_found` (a read *post-miss* — synthesise or fetch a default). The seam
was `LookupHooks<Db>` in `wavedb-core::hooks`; it was never wired into a read path.

## Why superseded

The hook seam was the right *instinct* but only half the story. Working the design
out revealed that transparent evolution across a node/client version skew needs a
whole mechanism the two hooks alone did not carry: a **naming convention** that
keeps a nested holder's `STRUCT_HASH` stable when a member evolves (numbered types
+ a `pub type` alias), **per-version addressing** by SALT derivation so old bytes
stay reachable and the holder is never rewritten, a **downgrade converter** for a
node serving an older client, a **collision guard**, and the discipline/warnings
around stranded intermediate versions.

[RFC 0040](0040-schema-migration-and-version-skew-DEPRECATED.md) folded these hooks
into that fuller design — and was then dropped in turn.

---

> **Note:** this idea migrated into a new, from-scratch RFC, which was itself
> deprecated: the engine runs **no** schema migration, by decision. See
> [RFC 0040](0040-schema-migration-and-version-skew-DEPRECATED.md).
