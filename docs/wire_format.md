# WaveDB Wire Format

A compact binary encoding for Rust values — the layout the `WaveWire` trait
implements. No `serde`, no `repr(C)`: the encoding is defined here, byte for byte.

> The `WaveWire` trait, its `#[derive(WaveWire)]`, and the built-in impls live in
> the standalone, dependency-free [`wavedb-wire`](../crates/wavedb-wire/README.md)
> crate.

Goal: a layout that can be reasoned about statically and shrinks the binary by
deleting serde's generic machinery. Each type has a compile-time-known **stack
size** and a heap section whose shape is described by its fields.

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

| Type                              | Stack bytes                    | Heap bytes                                                            |
| --------------------------------- | ------------------------------ | --------------------------------------------------------------------- |
| `u8..u128`, `i8..i128`, `f32/f64` | width, LE                      | —                                                                     |
| `bool`                            | 1 (`0`/`1`)                    | —                                                                     |
| `char`                            | 4 (`u32` scalar)               | —                                                                     |
| `[T; N]`                          | `N * T::STACK_SIZE`            | elements' heap, in order                                              |
| `String`                          | `u32` byte-length              | UTF-8 bytes                                                           |
| `Vec<T>`                          | `u32` region byte-length       | element units, back-to-back                                           |
| `Option<T>`                       | `1` flag                       | `T`'s full encoding (`stack`+`heap`) when `Some`; nothing when `None` |
| struct                            | sum of field stack sizes       | fields' heap, declaration order                                       |
| enum, all variants field-less     | 1 (tag)                        | —                                                                     |
| enum, any variant with fields     | 1 (tag) + `u32` payload length | variant fields as a unit                                             |
| tuple                             | sum of member stack sizes      | members' heap, in order                                               |

`usize`/`isize` are **not** encodable — the layout must not depend on the
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

## Trade-offs vs postcard

- No varints: integers cost their full width before compression. Downstream
  compression eats the constant zero runs; predictable offsets are worth more
  than pre-compression byte count.
- `Option<T>` uses **1 stack byte** (the flag) and puts `T`'s full encoding in
  the heap only when `Some`. `None` costs exactly 1 byte. Postcard uses 1 byte
  for None too, but its Some also wastes `T::STACK_SIZE` in the stack when `T` is
  large — this encoding doesn't.
- In exchange: single-allocation writes, zero-copy-friendly sequential reads,
  compile-time sizes, no serde/postcard code in the binary.
