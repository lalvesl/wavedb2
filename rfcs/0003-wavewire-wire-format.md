# RFC 0003 — The WaveWire wire format

- **Status:** Implemented
- **Crates:** `wavedb-wire`, `wavedb-wire-derive`
- **Code:** `crates/wavedb-wire/`; reference `docs/wire_format.md`

## Summary

WaveWire is a compact binary encoding for Rust values with a **compile-time
stack size** and a variable heap. A serialised value is two contiguous
sections — `[STACK: exactly T::STACK_SIZE bytes][HEAP: value.heap_size()
bytes]` — so serialisation allocates **once**
(`Vec::with_capacity(STACK_SIZE + heap_size())`) and every stack offset is a
compile-time constant. It is standalone: `wavedb-wire` has no `STRUCT_HASH` and
no engine coupling.

## Motivation

The [no-serde rule](0002-architectural-hard-rules.md) needs a replacement that
(a) shrinks the wasm binary by deleting serde's generic machinery, (b) can be
reasoned about statically, and (c) plays well with downstream compression. A
fixed stack/heap split gives all three: predictable zero runs for zstd to eat,
compile-time sizes, and sequential zero-copy-friendly reads.

## Design

**Stack section** — every fixed-width field packed little-endian in declaration
order, no padding. Every *dynamic* field still contributes a fixed slot (its
`u32` heap length, plus any flag/tag byte), keeping offsets constant.

**Heap section** — the dynamic payloads, appended depth-first in field order; a
parser walks it using the stack's `u32` length slots.

Per-type encoding (the full table is in `docs/wire_format.md`): integers/floats
cost their full width LE (no varints); `String`/`Vec<T>` carry a `u32`
region-length in the stack and bytes/elements in the heap; `Option<T>` is **1
stack byte** and `T`'s full encoding in the heap only when `Some`; enums are a
1-byte tag (+ `u32` payload length if any variant has fields).

**Two composition rules, different on purpose:**

1. **Flattened** (struct-in-struct, tuple member, array element, `T` in
   `Option<T>`): the child's stack slots inline into the parent's stack; its
   heap appends to the shared heap. This is what keeps offsets constant.
2. **Unit** (each `Vec<T>` element, an enum variant payload): the value is
   self-contained `[child stack][child heap]` inside the parent's heap. Elements
   parse sequentially with **no stored count** — the region length bounds them.

**`usize`/`isize` are not encodable** — the layout must never depend on the
platform's word size.

**Derive.** `#[derive(WaveWire)]` (in `wavedb-wire-derive`, re-exported by
`wavedb-wire`) emits the encode/decode. Gotcha: it emits absolute
`::wavedb_wire::` paths, so any crate using it needs `wavedb-wire` as a direct
dependency.

**Validation feature.** The optional `validation` feature adds
`to_wire_checked`/`from_wire_checked` — a `[crc32][wire]` frame. Every
disk/transport boundary uses it; no structure hand-rolls a byte layout. Decode
failure is **size-only** (a length mismatch), asserted to a specific error
variant in tests.

## Alternatives & prior art

Compared against **postcard** (`docs/wire_format.md`): postcard's varints save
pre-compression bytes but cost predictable offsets, and its `Some` still wastes
`T::STACK_SIZE` in the stack for large `T`. WaveWire trades raw byte count for
single-allocation writes, compile-time sizes, and no serde/postcard code in the
binary — the constant zero runs are recovered by zstd downstream
([RFC 0018](0018-storage-engine.md)).
