# RFC 0039 — Developer experience (M9)

- **Status:** Planned (low priority) — the M9 milestone
- **Crates:** tooling / templates / docs (no single crate)
- **Depends on:** [RFC 0040](0040-schema-migration-and-version-skew-PLANNED.md),
  [RFC 0002](0002-architectural-hard-rules.md)

## Summary

The on-ramp for building *someone else's* app on WaveDB: a project template, a
"Building an app on WaveDB" guide with a schema-evolution cookbook, and the
versioning policy that starts at first release.

## Motivation

Every mechanism exists (the whole M1–M8 stack), but nothing tells a newcomer how
to assemble a schema/server/node/client/web workspace, wire the side-features
([RFC 0017](0017-exposure-registry-and-side-features.md)), or evolve a schema in
practice. WaveDB is usable by its authors and opaque to everyone else; M9 closes
that gap. It is low priority only because it gates *adoption*, not *correctness* —
the engine is done without it.

## Design (target)

- **`cargo-generate` template** — a schema/server/node/client/web workspace
  skeleton with one struct per shape (Unique / NonUnique / natural-keyed), hook
  examples, and a dev-cluster stub. Encodes the correct crate split and the
  `server-side` / `client-side` feature wiring
  ([RFC 0017](0017-exposure-registry-and-side-features.md)) so a newcomer starts
  compliant.
- **"Building an app on WaveDB" guide** + a **schema-evolution cookbook** — the
  numbered-type + `pub type` alias convention, `UpgradeFrom` / `DowngradeFrom`, and
  the `prefer_current` / `upgrade_on_miss` hooks
  ([RFC 0040](0040-schema-migration-and-version-skew-PLANNED.md)) shown against a
  concrete `Task1`→`Task2` change, plus the `expose_server!` / `expose_client!`
  allowlist as a security surface.
- **Versioning policy** — the point where `FORMAT_VERSION` is unpinned from `1`
  and version discipline begins ([RFC 0002 §8](0002-architectural-hard-rules.md)).
  Until then, on-disk layouts change freely; M9 is where that stops.

## Prior art in the repo

The examples (`todo-app`, `contact-book`, `showcase`) already demonstrate the
end-to-end shape; M9 is largely about *packaging* what they prove into a template
and a narrative, not inventing new mechanism.
