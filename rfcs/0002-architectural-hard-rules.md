# RFC 0002 — Architectural hard rules

- **Status:** Accepted (enforced by tooling)
- **Crates:** all
- **Reference:** `docs/development_standards.md`, `clippy.toml`,
  `scripts/check_file_length.sh`

## Summary

A short list of rules that are **load-bearing on the architecture**, not style
preferences. Breaking one is a design change, not a refactor. Most are enforced
by tooling so "is this compliant?" is answered by running the checks, not by
taste.

## The rules

### 1. No `dyn`, no sum-type registries

All dispatch is a **generated `match` on the 64-bit `STRUCT_HASH`** to concrete,
monomorphized arms. No trait objects, no fn-pointer tables, no runtime
registration, no `enum` of "all known types". This holds inside macro
expansions too. *Why:* the whole point is that the compiler sees every type
concretely — monomorphized code, no vtable indirection, dead code eliminated per
deployment. A registry is a `match`, and the `match` *is* the security surface
(see [RFC 0017](0017-exposure-registry-and-side-features.md)).

### 2. No serde

Byte layouts are the **WaveWire** codec ([RFC 0003](0003-wavewire-wire-format.md)),
defined byte-for-byte in `docs/wire_format.md`, little-endian, platform
independent — `usize`/`isize` are **never** encodable. *Why:* serde's generic
machinery bloats the wasm binary and hides the layout; WaveWire is a fixed
stack/heap split the compiler knows the size of.

### 3. `async` end to end

Every public surface is async — native on Tokio, wasm on
`wasm_bindgen_futures`. No blocking I/O behind an async signature. The engine's
futures are deliberately **non-`Send`** (current-thread `LocalSet` model);
`#![allow(clippy::future_not_send)]` at the crate root is the established stance
in core/storage/quick-node.

### 4. Identity-load-bearing dependencies are pinned exactly

If an algorithm's output is persisted or shared across builds, an unreviewed
version bump is corruption. `seahash` is pinned **`=4.1.0`** because
`STRUCT_HASH` identity depends on its exact output. Pins carry a comment stating
the reason.

### 5. Typed errors per layer, converted at the seam

One `thiserror` enum per layer (`wavedb_wire::Error`, `wavedb_core::Error`,
`StorageError`, the net/node/client errors), each carrying **evidence** (the id
that dangled, `need`/`have` counts). **Fabricating a foreign layer's error
inline is forbidden** — convert at the documented boundary
(`StorageError → core::Error::Backend`, core → `NodeError::from_core`, …). No
`unwrap`/`expect`/`panic!` in library paths.

### 6. File budget: 350 non-test lines per `.rs`

Enforced by `scripts/check_file_length.sh`; the count stops at the first
`#[cfg(test)]`, so colocated tests are free. Over budget ⇒ split **by layer**
(a codec apart from the thing it encodes). Every module opens with a `//!`
header stating its *one* responsibility — if the header needs "and", the module
wants splitting.

### 7. Complexity thresholds live in `clippy.toml` only

`cognitive_complexity`, `too_many_lines`, `too_many_arguments` run as `warn`
under pedantic+nursery and are treated as failures. The numbers are in
`clippy.toml` — a single source of truth — and **ratchet down only**. A function
over budget is split into named phases, never `#[allow]`-ed.

### 8. No format versioning pre-release (policy)

`FORMAT_VERSION` is pinned at `1`. On-disk and wire layouts change **freely**
between commits — no bump, no migration notes. An old `data.bin` is simply
unsupported (delete it; journal replay rebuilds pages). Version discipline
begins at the first release.

## Why collect them here

These rules recur as constraints in nearly every other RFC ("no `dyn`, so
dispatch is a `match`"; "no serde, so the wire is hand-emitted"). Stating them
once lets the rest of the corpus reference `RFC 0002 §N` instead of re-arguing.
