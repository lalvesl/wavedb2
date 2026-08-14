# RFC 0045 — Vector search

- **Status:** Planned — opened 2026-07-28
- **Crates:** `wavedb-macros`, `wavedb-core`, `wavedb-storage`
- **Code (target):** a new index kind beside `#[wavedb::pivot(...)]`
- **Builds on:** [RFC 0011](0011-bptree-index-and-collections.md) (secondary
  indexes, `Bound::Prefix`), [RFC 0018](0018-storage-engine.md) (the dictionary's
  no-trainer sampling, reused for centroids)

## Summary

Nearest-neighbour search declared like any other index — `#[wavedb::vector(field,
metric = cosine)]` — adding one root to the collection's `Pivot` and one typed
lookup to its handle:

```rust
#[wavedb(NonUnique)]
#[wavedb::vector(embedding, metric = cosine)]
pub struct Note { pub embedding: [f32; 768], pub body: String }

// Streams (Id, score), best first — an async iterator like every other read.
let mut hits = notes.near_embedding(&db, &query, 10);
```

Underneath: **IVF over the existing `BpTree`**. A small per-tenant centroid table
partitions the vector space; each vector's centroid id prefixes its key in an
ordinary `BpTree<SecKey>`, so a probe is a `Bound::Prefix` scan — machinery that
already exists — and a search costs a handful of page reads instead of a scan.

## Motivation

Vector search is where "which database do I add?" usually splits an application
stack in two. WaveDB's whole thesis is that the schema *is* the protocol and the
DB ships as a library; bolting on a separate vector service would reintroduce
exactly the DTO/sync seam the project exists to remove. Embeddings also live
naturally beside the record they describe — the same tenant, the same collection,
the same permission.

**And WaveDB's grain makes the easy structure the right one.** Indexes here are
per tenant, per collection. A B2C tenant holds thousands of notes, not a billion;
a B2B tenant holds hundreds of thousands. At that scale the sophistication of a
graph index buys little and costs a lot — which is the whole argument below.

## Design

### The shape: IVF, not HNSW

| | HNSW | IVF + prefix scan |
|---|---|---|
| Search IO | pointer chase, many random reads | `n_probe` posting lists = `n_probe` page reads |
| Insert | graph surgery, touches many nodes | one key into one `BpTree` — same as any secondary |
| Rebuild | expensive, global | re-key a centroid's members, incremental |
| Fits the engine | needs its own on-disk format | **is** a `BpTree<SecKey>` |
| RAM | the graph wants to be resident | centroid table only (KiB) |

HNSW wins on recall-per-IO at billion scale with the index in RAM. Neither
condition holds here: the scarce resources are RAM and IOps, the per-tenant sets
are small, and every new on-disk format is a new thing to make crash-safe. IVF
maps onto structures the engine already has.

### Keys

A vector index is a `BpTree<SecKey>` whose key is

```
[ centroid: u16 BE ][ quantized code: N bytes ][ record LocalId ]
```

- the **centroid prefix** makes a probe a `Bound::Prefix` scan — already
  implemented, already page-ordered;
- the **quantized code** (product or scalar quantization) rides *in the key*, so
  a probe scores candidates straight off the index pages without touching a
  single record;
- the **`LocalId`** keeps keys unique and gives the rerank step its address.

Search: score the query against the centroid table in RAM → prefix-scan the
`n_probe` nearest → score candidates from their codes → fetch the top `k`
records and compute the exact metric. Reads scale with `n_probe`, not with the
collection.

### Centroids without a training pass

The centroid table is learned exactly the way the zstd dictionary is: an
**append-only, capped sample**. The first vectors inserted become the initial
centroids; subsequent inserts assign to the nearest and, while the table is below
its cap, split the most dispersed cell. No offline trainer, no rebuild step, and
— like `dict_len` — the table's state is versioned by its own length, so keys
written under an older table stay readable.

The table lives in the `Pivot` alongside the roots (it is small and rewritten
rarely, which is exactly what the `Pivot` is for).

### Writes reuse the secondary-index rule

`insert` indexes the vector; `save` re-keys **only if the vector field changed**
(the existing `old_key == new_key` check); `remove` de-indexes. All inside the
same atomic batch, all through machinery that already exists.

### Per-tenant centroids are a privacy property, not just a simplification

Centroids trained across tenants would be strictly better clusters — and would
leak one tenant's distribution into another's index. WaveDB's invariant is that
tenants never share a structure; keeping the centroid table per-tenant preserves
it, and at per-tenant scale the recall difference is small. This is a deliberate
trade, recorded so nobody "optimises" it away later.

## Open questions

- **Metrics.** Cosine and L2 to start; dot-product needs a normalisation story.
- **Dimension in the type.** `[f32; N]` is already `WaveWire`-encodable, so the
  dimension can be a const on the field — but folding it into `STRUCT_HASH`
  means changing the dimension is a new type (correct, and worth stating).
- **Quantization scheme and code width** — the recall/RAM/IO knob; needs a
  measured default, not a guessed one.
- **`n_probe` as an API surface**: a per-call argument, a per-index default, or
  an adaptive "keep probing until the k-th score stops improving"?
- **Filtered vector search** (nearest *among* records matching a predicate) is
  the query that breaks every vector DB. Here it composes: a `#[server]` body can
  `intersect` the vector index's `Id` stream with a secondary index's, using the
  set algebra `IdStreamExt` already defines. Worth proving early — it may be the
  most compelling thing about doing this inside WaveDB at all.
- **Recall testing.** This is the first WaveDB index whose answer is
  *approximate*; the test suite needs a ground-truth brute-force comparison and a
  recall floor, which is a new kind of test for this repo.
