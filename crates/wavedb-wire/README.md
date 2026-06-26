# wavedb-wire

The standalone **`Wire` codec** — pure value ⇄ bytes, zero WaveDB coupling.

> For the project-wide idea and quickstart see the
> [root README](../../readme.md). For how the layout is specified see
> [`docs/wire_format.md`](../../docs/wire_format.md).

## What it is

`wavedb-wire` is the `Wire` trait plus its built-in impls (scalars, `bool`,
`char`, `String`, `Vec`, `Option`, arrays, tuples), the two free functions, and a
`#[derive(WaveWire)]`:

```rust
#[derive(wavedb_wire::WaveWire)]
struct Point { x: u64, y: u64 }

let bytes: Vec<u8> = wavedb_wire::to_wire(&value);
let value: T       = wavedb_wire::from_wire::<T>(&bytes)?;
```

That is the whole surface. The crate knows nothing about the rest of WaveDB.

### `#[derive(WaveWire)]`

Re-exported from the companion proc-macro crate
[`wavedb-wire-derive`](../wavedb-wire-derive) (the serde / serde_derive split — a
proc-macro must live in its own crate). It generates a `Wire` impl for:

- **structs** — named, tuple, or unit: fields encode in declaration order.
- **enums** — the canonical tag form. All variants field-less ⇒ a single `u8`
  tag; any variant with fields ⇒ `tag (u8) + payload-length (u32)` in the stack
  and the active variant's fields as a unit in the heap. Tags are the variant
  declaration order.

The derive emits absolute `::wavedb_wire::` paths, so it works from any crate that
depends on `wavedb-wire`.

## What it deliberately does *not* do

- **No `STRUCT_HASH`, no registry, no envelope.** The bytes carry no type
  identity. `from_wire::<T>` does not check "is this really a `T`?" — it just
  reads a `T` out of the buffer. Type identity / dispatch is a higher layer
  (`wavedb-core` + the generated `Object` enum), not this one.
- **No type validation.** The only ways a decode fails are layout/shape
  mismatches: the buffer is **too short for the type's size**
  (`Error::UnexpectedEof` — the dominant case) or an intrinsic per-type check
  trips (`InvalidBool`, `InvalidChar`, `Utf8`, `InvalidTag`).

So decoding the wrong type against a long-enough buffer does not error — it
yields some other value. Distinguishing types is the caller's job (it picks `T`).

## Layout (one-line recap)

A value is `[ STACK (T::STACK_SIZE bytes, fixed) ][ HEAP (variable) ]`. Every
fixed-width field is packed little-endian in the stack at a compile-time offset;
dynamic fields keep a `u32` length / flag slot in the stack and put their payload
in the heap. Serialisation allocates once. Full spec in
[`docs/wire_format.md`](../../docs/wire_format.md).

## Relationship to `wavedb-core`

`wavedb-core` depends on this crate and re-exports it as `wavedb_core::wire`, so
existing `wavedb_core::wire::{Wire, Cursor, to_wire, from_wire}` paths and the
`#[derive(WaveWire)]` codegen keep working unchanged. `wavedb_core::Error` wraps
this crate's `Error` (`#[from]`). The `Wire` impls for WaveDB's own types (`Id`,
`LocalId`, `U48`, `Metadata`, `PermissionRef`, …) live in `wavedb-core`, where
those types are defined.
