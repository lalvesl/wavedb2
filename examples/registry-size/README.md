# registry-size

The measuring stick for the M1 risk item — *"registry code grows the wasm
binary as schemas grow"* — closed in M5. Sixty-four `#[wavedb]` structs are
always defined; the `expose_client!` width is feature-selected (1 / `n16` /
`n64`), and a wasm export probes the registry with a **runtime** hash so
every `match` arm and its decode survive dead-code elimination. Because
unexposed definitions are dead code, the size **delta between widths is
exactly what one more exposure line costs**.

Run it:

```sh
scripts/registry_size.sh
```

## Latest numbers (2026-07-10, rust 1.96, cargo `wasm-release`, pre-bindgen)

| exposed structs | raw (B) | gzip (B) |
| --------------- | ------- | -------- |
| 1               | 464 359 | 108 506  |
| 16              | 467 421 | 109 170  |
| 64              | 468 549 | 110 065  |

Marginal cost per exposed struct:

| step    | raw   | gzip |
| ------- | ----- | ---- |
| 1 → 16  | 204 B | 44 B |
| 16 → 64 | 23 B  | 18 B |

## Reading the two slopes

The structs cycle only **four** field shapes, so LLVM merges the
structurally identical `WaveWire` decode fns. That split is the point:

- **16 → 64 (~23 B raw / arm)** — the pure registry cost: one more hash
  compare + branch in the `match`, the shape's decode already in the
  binary. This is the number the M1 risk asked about, and it is nothing
  like a sum type or a descriptor table.
- **1 → 16 (~204 B raw / struct)** — what a struct with a genuinely novel
  shape pays: the arm **plus** its monomorphized `WaveWire` decode — code a
  heterogeneous schema needs anyway to speak its own types.

Scope: all structs are Unique and only the client registry (`knows` +
`decode_check`) is anchored — the transport/engine stack is measured
separately by `scripts/wasm_size.sh`. A NonUnique adds its generated
Pivot/BpTree types on top, which is schema machinery, not registry growth.
