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

## Engine record layout

The codec above is engine-agnostic (the `wavedb-wire` crate has no `STRUCT_HASH`
and no engine coupling), but the node and the client cache stack a few fixed
envelopes on top of it. Every stored value opens with an 8-byte little-endian
`STRUCT_HASH` head — storage routes on it and decode verifies it, so a stale or
foreign `Id` can never decode as the wrong type. Three envelope forms follow:

- **bare** (`Pivot` records — pure addressing, no history):
  `[STRUCT_HASH (8)][WaveWire bytes]`.
- **record** (Unique + NonUnique user data):
  `[STRUCT_HASH (8)][meta_len (u32 LE)][WaveWire(Metadata)][WaveWire body]` — the
  `meta_len` prefix splits two independently-decodable payloads, and carrying the
  `Metadata` header is what makes every version chainable.
- **B+tree node**: `[BPTREE_NODE_HASH (8)][kind (u8)][WaveWire bytes]`.

### `Metadata`

The per-record header records **who wrote a version, when, and under which
rule** — state review, not domain data (a domain fact like "member since"
belongs in the record's own fields). It rides `WaveWire` field-by-field like
everything else; its stack is a fixed **26 bytes**:

| Field            | Type                    | Stack | Heap (when `Some`)        |
| ---------------- | ----------------------- | ----- | ------------------------- |
| `previous`       | `Option<u64>`           | 1     | 8 (predecessor's instant) |
| `succession`     | `Succession`            | 9     | —                         |
| `pivot_id`       | `Option<LocalId>`       | 1     | 10 (owning Pivot)         |
| `user`           | `U48`                   | 6     | —                         |
| `device_created` | `u64`                   | 8     | —                         |
| `permission`     | `Option<PermissionRef>` | 1     | variable                  |

`Succession` (`CreatedAt(u64)` on the live version, `Next(u64)` on an archive)
is **hand-encoded** as a fixed 9-byte stack — `tag (1) + instant (8 LE)` —
rather than the derive's enum form: the payload never varies, so the derive's
`u32` length prefix would be dead weight on every stored record. A Unique first
version (every `Option` field `None`) is the minimal case: 26 stack bytes, zero
heap.

Chain links are **authoring instants, not addresses**: `previous` and
`Succession::Next` carry the `u64` instant a version was written, and an
archive's slot is a pure function of `(type, shape, instant)` — so a link
written once stays correct forever and no archive is ever repointed. The slot
derivation lives in `crates/wavedb-core/src/record.rs`.

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
