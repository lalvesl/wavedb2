# wavedb-wire

A small, standalone **`WaveWire` codec**: turn a value into bytes and back.

```rust
let bytes: Vec<u8> = wavedb_wire::to_wire(&value);        // T       -> Vec<u8>
let value: T       = wavedb_wire::from_wire::<T>(&bytes)?; // Vec<u8> -> T
```

That is the whole job. The crate has **one** dependency (`thiserror`) — plus
`crc32fast` when the optional `validation` feature is on (below).

## The trait and the derive

`WaveWire` is the trait every encodable type implements; `#[derive(WaveWire)]`
generates it. The trait and the derive share the name (the same pattern as
`Clone`):

```rust
#[derive(wavedb_wire::WaveWire)]
struct Point { x: u64, y: u64 }
```

Built-in impls cover the common types: integers and floats, `bool`, `char`,
`String`, `Vec`, `Option`, arrays, and tuples. The derive handles:

- **structs** — named, tuple, or unit: fields encode in declaration order.
- **enums** — all variants field-less ⇒ a single `u8` tag; any variant with
  fields ⇒ `tag (u8) + payload length (u32)` plus the active variant's fields.
  Tags follow variant declaration order.

The derive emits absolute `::wavedb_wire::` paths, so it works from any crate that
depends on `wavedb-wire`.

## Layout

A value is two contiguous parts:

```
[ STACK — T::STACK_SIZE bytes, fixed ][ HEAP — variable ]
```

Fixed-width fields pack little-endian into the stack at compile-time offsets;
dynamic fields (`String`, `Vec`, …) keep a `u32` length slot in the stack and put
their payload in the heap, so serialisation allocates once. Full spec in
[`docs/wire_format.md`](../../docs/wire_format.md).

## Feature `validation` — crc32-framed encoding

For bytes that cross an unreliable boundary (disk, transport), the `validation`
feature adds a checked pair that frames the plain encoding with a CRC32:

```rust
let bytes = wavedb_wire::to_wire_checked(&value);           // [crc32 4 B LE][stack][heap]
let value: T = wavedb_wire::from_wire_checked::<T>(&bytes)?; // verify crc, then decode
```

- The prefix is a little-endian `u32` CRC32 of the `[stack][heap]` payload;
  `CRC_PREFIX_LEN` (= 4) names the framing size. Encoding stays one allocation
  (the slot is reserved, then patched).
- A mismatch fails as `Error::CrcMismatch { stored, computed }` **before** any
  decode; a buffer shorter than the prefix as `Error::UnexpectedEof`.
- The two framings don't mix: plain `from_wire` on a checked buffer would read
  the CRC bytes as the value's stack. Pick one per boundary.
- Integrity only, not identity: the CRC says the bytes weren't corrupted, not
  that they encode the `T` you asked for (that stays the caller's job).

`wavedb-core` forwards the feature (`wavedb-core/validation`) and re-exports the
pair via `wavedb_core::wire`.

## What it does *not* do

- **The bytes carry no type tag.** `from_wire::<T>` does not check "is this really
  a `T`?" — it just reads a `T` out of the buffer. Picking the right `T` is the
  caller's job; this crate only moves bytes.
- **No validation beyond layout.** A decode fails only on a size/shape mismatch:
  the buffer is too short (`Error::UnexpectedEof`, the common case) or an intrinsic
  per-type check trips (`InvalidBool`, `InvalidChar`, `Utf8`, `InvalidTag`).
  Decoding the wrong type against a long-enough buffer yields some other value, not
  an error.
