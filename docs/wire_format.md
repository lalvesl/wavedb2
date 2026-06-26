# WaveDB Wire Format (replaces serde + postcard)

> The `Wire` trait and its built-in impls live in the standalone, dependency-free
> [`wavedb-wire`](../crates/wavedb-wire/README.md) crate (no `STRUCT_HASH`, no
> registry — pure value ⇄ bytes). `wavedb-core` re-exports it as
> `wavedb_core::wire`. The record **envelope** (the `STRUCT_HASH` prefix, below)
> and WaveDB's own type impls (`Id`, `Metadata`, …) stay in `wavedb-core`.

Goal: shrink the WASM binary by deleting serde's generic machinery, and give the
engine a layout it can reason about statically. Each `STRUCT_HASH` has a
compile-time-known **stack size** and a heap section whose shape is described by
the struct's field descriptors. The layout is defined entirely by `Wire` — no
`serde`, no `repr(C)`.

## Layout

A serialised value is two contiguous sections:

```
[ STACK section — exactly T::STACK_SIZE bytes, known at compile time ]
[ HEAP section  — variable bytes, length = value.heap_size()          ]
```

- **Stack section**: every fixed-width field packed little-endian in
  declaration order, no padding. Every _dynamic_ field contributes a fixed
  slot to the stack section too (its `u32` heap-length, plus flag/tag bytes),
  so all stack offsets are compile-time constants.
- **Heap section**: the dynamic payloads, appended in depth-first field
  declaration order. A parser walks it sequentially using the `u32` length
  slots from the stack section.

Serialisation allocates **once**: `Vec::with_capacity(T::STACK_SIZE +
value.heap_size())`.

## Per-type encoding

| Type                              | Stack bytes                             | Heap bytes                                                        |
| --------------------------------- | --------------------------------------- | ----------------------------------------------------------------- |
| `u8..u128`, `i8..i128`, `f32/f64` | width, LE                               | —                                                                 |
| `bool`                            | 1 (`0`/`1`)                             | —                                                                 |
| `char`                            | 4 (`u32` scalar)                        | —                                                                 |
| `Id`                              | 16                                      | —                                                                 |
| `LocalId`                         | 10 (`KEY u64 LE` + `FLAG\|SALT u16 LE`) | —                                                                 |
| `[T; N]`                          | `N * T::STACK_SIZE`                     | elements' heap, in order                                          |
| `String`                          | `u32` byte-length                       | UTF-8 bytes                                                       |
| `Vec<T>`                          | `u32` region byte-length                | element units, back-to-back                                       |
| `Option<T>`                       | `1` flag                                | `T`'s full wire (`stack`+`heap`) when `Some`; nothing when `None` |
| struct                            | sum of field stack sizes                | fields' heap, declaration order                                   |
| enum, all variants field-less     | 1 (tag)                                 | —                                                                 |
| enum, any variant with fields     | 1 (tag) + `u32` payload length          | variant fields as a unit                                          |
| tuple                             | sum of member stack sizes               | members' heap, in order                                           |

`usize`/`isize` are **not** encodable — wire layout must not depend on the
platform.

## Composition rules

Two ways a value nests, and they are different on purpose:

1. **Flattened** (struct field inside a struct, tuple member, array element,
   the `T` inside `Option<T>`): the child's stack slots are emitted inline
   into the parent's stack section; the child's heap payloads are appended to
   the shared heap section in field order. This is what keeps every stack
   offset a compile-time constant.
2. **Unit** (each `Vec<T>` element, an enum's variant payload): the value is
   self-contained — `[child stack][child heap]` back-to-back inside the
   parent's heap region. Elements parse sequentially: read `T::STACK_SIZE`
   bytes, the child's own length slots say how much heap follows, repeat
   until the region is exhausted (the region length is the parent's `u32`
   slot, so no element count is stored).

## Record envelope and the registry header

A top-level record is prefixed with its `STRUCT_HASH` (`u64`):

```
[ u64 STRUCT_HASH LE ][ stack ][ heap ]
```

`STRUCT_HASH` is the compile-time `const` hash of the struct's name, shape, and
field names/types (see [`wavedb-core`](../crates/wavedb-core/README.md#struct_hash)),
so it identifies both the type **and** its schema generation — there is no
separate version byte. On read, a stored `STRUCT_HASH` that differs from the
reader's compiled head simply means a different type; bridging it is the
application's job via the `first_try` / `fallback_not_found` hooks.

All nodes (server and client/WASM) share a **static registry generated in
`build.rs`** — a scanner walks the schema crate, finds every `#[wavedb]` struct,
and emits an `Object` enum (`STRUCT_HASH` → variant) spliced in with `include!`.
The enum's inherent `from_wire` / `to_wire` / `struct_hash` are the dispatch: from
a `STRUCT_HASH` (read off the wire envelope or the `Id`) a `match` selects the
concrete arm, monomorphised — **no `dyn`** — giving the wire parser, the
`first_try` / `fallback_not_found` hooks, and the `Pivot`/`BpTree` accessors. A
type's static facts (`STACK_SIZE`, `SHAPE`) are inherent `const`s reached through
that arm — no descriptor table. See
[`wavedb-macros`](../crates/wavedb-macros/README.md#the-registry--generated-in-buildrs).

## Trade-offs vs postcard

- No varints: integers cost their full width before compression. The
  per-STRUCT dictionary compressor eats the constant zero runs; predictable
  offsets are worth more than pre-compression byte count.
- `Option<T>` uses **1 stack byte** (the flag) and puts `T`'s full wire
  representation in the heap only when `Some`. `None` costs exactly 1 byte.
  Postcard uses 1 byte for None too, but its Some also wastes `T::STACK_SIZE` in
  the stack when T is large — this encoding doesn't.
- `LocalId` (10 bytes) is used inside `Option<LocalId>` in `Metadata` instead of
  a full `Id` (16 bytes): the BpTree is tenant-scoped so `TENANT (u48)` is
  redundant. `Metadata` stack = 18 bytes; heap grows only for fields that are Some.
- In exchange: single-allocation writes, zero-copy-friendly sequential reads,
  compile-time sizes, no serde/postcard code in the binary.
