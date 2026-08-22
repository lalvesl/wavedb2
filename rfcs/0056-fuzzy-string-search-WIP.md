# RFC 0056 — Fuzzy string search

- **Status:** WIP — opened 2026-08-01. **Landed 2026-08-01**: the text layer
  (normalization, grams, filters, verify), the `#[wavedb::fuzzy]` declaration,
  the posting trees and their maintenance, and the engine-side read
  (`fuzzy_<field>`), demonstrated by `search_todos` in the todo-app example.
  Two corrections to this document are marked inline (the fixed-width
  justification, and Jaccard-as-type-ahead). **Not** shipped: a wire command,
  so a client reaches this through a `#[server]` function — which is the shape
  the todo-app has anyway.
- **Crates:** `wavedb-macros`, `wavedb-core`
- **Code (target):** a new declared index kind beside `#[wavedb::pivot(...)]`
- **Builds on:** [RFC 0011](0011-bptree-index-and-collections.md) (secondary
  indexes, `Bound::Prefix`), [RFC 0004](0004-struct-hash-and-schema-evolution.md)
  (the declaration folds into the identity)
- **Related:** [RFC 0045](0045-vector-search-PLANNED.md) is the same move for a
  different distance — a declared index kind whose lookup reduces to a prefix
  scan over machinery that already exists; [RFC 0051](0051-ordered-record-lists.md)
  is the contrast on write cost (see "What a posting does not carry")

## Summary

Approximate string matching declared like any other index —
`#[wavedb::fuzzy(name)]` — adding one `BpTree<SecKey>` root to the collection's
`Pivot` and one typed lookup to its handle:

```rust
#[wavedb(NonUnique)]
#[wavedb::fuzzy(name)]
pub struct Contact { pub name: String, pub email: String }

// Ranked, best first.
let hits = contacts.fuzzy_name(&db, "jhon smtih", Fuzzy::similarity(0.4)).await?;
let hits = contacts.fuzzy_name(&db, "jhon", Fuzzy::distance(2)).await?;
```

Underneath: an **n-gram posting index over the existing `BpTree`**. The indexed
string is normalized, padded, cut into n-grams, and each gram becomes a key
`[gram][len][anchor]` in an ordinary `BpTree<SecKey>`. A query decomposes the same
way, so looking up one gram is a `Bound::Prefix` scan — machinery that already
exists — and the answer is the anchors that share enough grams, verified by
actual edit distance.

## Motivation

Every application that stores a name, a title or a tag eventually needs "find
this, roughly". Today WaveDB answers exact and range (`#[wavedb::pivot]` +
`Bound`), which means a typo, a transposition or a missing accent returns
nothing at all. The workarounds an application reaches for are worse than the
feature: fetch everything and filter in the client (defeats the index), or add a
search service beside the database (reintroduces exactly the sync seam WaveDB
exists to remove — the schema is the protocol, there is no DTO layer).

**And the grain makes the easy structure the right one**, which is the same
argument RFC 0045 makes for vectors. Indexes here are per tenant, per
collection. A B2C tenant holds thousands of contacts; a B2B tenant holds
hundreds of thousands. At that scale a posting list is a handful of dense
reads, and the sophistication of an automaton buys little against what it costs
to maintain.

## Design

### The key: a gram, a length, an anchor

The indexed value is normalized (below), padded with `n-1` sentinels at each
end, and cut into n-grams — a string of `L` codepoints yields `L + n - 1` grams.
Default `n = 3`.

Each gram becomes one `SecKey`:

```
field = [c0 u32 BE][c1 u32 BE][c2 u32 BE][len u8]     // 13 bytes at n = 3
rec   = the record's anchor LocalId
```

Three decisions in that layout:

- **Codepoints, fixed width, not UTF-8 bytes.** ~~A variable-width gram is not a
  safe `Bound::Prefix`: the encoding of `ab` is a byte prefix of the encoding of
  `abc`, so a scan for one would silently return the other.~~ **Corrected
  2026-08-01 during implementation — that reason is wrong.** `n` is fixed per
  index (it folds into the identity), so all grams have the same *codepoint*
  count, and UTF-8 is self-synchronizing: if one such gram's bytes prefixed
  another's, decoding would yield the same `n` codepoints and they would be the
  same gram. Checked exhaustively over a mixed-width alphabet — zero collisions
  (`utf8_is_prefix_safe_too_which_is_not_why`).

  The reasons that do hold: **the sentinel cannot be encoded in UTF-8 at all**
  (padding needs a symbol outside the alphabet; `0xFFFF_FFFF` is above the
  Unicode range, where any character encoding would need an in-band escape and
  the escaping rules that follow) — decisive on its own; the **length byte has a
  known home**, since reading it as "the last byte" is only sound when
  everything before it is exactly `4n`; and the **scan prefix is a constant**
  rather than a length re-derived per query. Four bytes per codepoint costs 12
  bytes where 3–6 would do, and space is the abundant resource.
- **The length rides in the key**, after the gram and before the anchor, so the
  prefix scan reads it **for free** — it is already in the leaf bytes the descent
  landed on. It buys the length filter (next section) at zero extra IO. It
  saturates at 255, which is sound in one direction only and that is the one we
  need: saturation compresses the high end, so the computed gap never *exceeds*
  the true gap, so a rejection is never a false negative.
- **The anchor is the record**, as in every `SecKey` — the posting names where
  the record lives and nothing more.

### The read: prefix scans, then two filters, then verify

1. Decompose the query into its `T = q + n - 1` grams, deduped.
2. One `Bound::Prefix` scan per gram — one descent (two or three nodes cold) plus
   the posting run. Merge into a per-anchor count in RAM.
3. **The count filter.** Each edit destroys at most `n` grams, so a string within
   edit distance `k` of the query shares at least `T - n*k` grams with it.
   Anchors below that are rejected without being read. This is a *lower* bound —
   it admits false positives, never false negatives, which is what makes the
   result exact after step 5.
4. **The length filter.** `ed(a,b) ≥ | |a| - |b| |`, and the length is already in
   hand from step 2. Free rejection.
5. **Verify.** Fetch each survivor at its anchor and compute the real distance
   (or trigram Jaccard). One random read each — **this is the dominant cost**,
   and steps 3 and 4 exist entirely to make the survivor set small.

~~Two query modes~~ **Three** modes come off the same postings — the third was
added during implementation, because the second does not do what this RFC
claimed:

- `Fuzzy::distance(k)` — exact edit distance, the count filter parameterised
  by `k`.
- `Fuzzy::similarity(t)` — trigram Jaccard `|G(q) ∩ G(s)| / |G(q) ∪ G(s)| ≥ t`.
  ~~the "search as you type" behaviour~~ **Corrected 2026-08-01**: Jaccard is
  **symmetric**, so it divides by the union and a short query cannot score well
  against a long record however perfectly it matches. Measured: `"milk"` against
  `"Buy milk"` scores **0.33**, `"rust"` against `"Read the Rust book"` scores
  **0.13** — and an unrelated pair scores 0.0, so any threshold loose enough to
  accept 0.13 accepts nearly everything. It is the right measure for "are these
  two strings alike?" (de-duplication, "did someone already add this?") and the
  wrong one for type-ahead.
- `Fuzzy::contains(t)` — trigram **containment** `|G(q) ∩ G(s)| / |G(q)| ≥ t`,
  **asymmetric**: how much of what the caller typed appears in the record. A
  perfect substring is `1.0` whatever the record's length. Same measured pairs:
  `"milk"` → **0.67**, `"rust"` → **0.50**, `"mlk"` (a dropped letter) → **0.40**,
  unrelated → **0.00**. This is the type-ahead mode, and it has a pleasing
  property the other two lack: the score **is** the prefilter's ratio, so the
  count filter is not a bound to be verified — it is already the answer, and the
  record is read only to return it.

Results are **ranked, therefore buffered**: a best-first order cannot be known
until the last candidate is scored, which is the same honesty as `All` buffering
over the wire rather than pretending to stream.

### What a posting does not carry

A posting holds a gram, a length and an anchor — **no record bytes**. That is the
whole difference from a declared list (RFC 0051), and it buys back the rule that
RFC 0051 could not have:

> A save whose indexed field did not change writes **nothing** to this index.

RFC 0051 must rewrite a record in every list unconditionally, because the list
*duplicates the record* and any other field moving makes the copy stale. Here the
posting set is a pure function of the indexed field, so an unchanged field means
an identical posting set and there is nothing to write. When the field does
change, only the **symmetric difference** of the two gram sets moves — not a
full remove-and-reinsert.

### The write cost, stated plainly

Inserting a record writes `L + n - 1` keys into the tree, scattered across the
key space, so it touches roughly that many leaf pages. A 20-character name is
~22 inserts. Removal is symmetric. All of it lands in the collection's single
atomic batch, so it is one barrier — but it is not one page.

That is the price, and it is the honest headline of this RFC the way "one full
copy of every record per declaration" is 0051's. What makes it acceptable is the
grain: these are per-tenant, per-collection trees of a few levels, and the
alternative structures below cost *more* per write, not less.

### Normalization, and the dependency it does not take

Normalization reaches stored bytes, so it is declared and folded, and it must be
cheap enough for a wasm artifact:

- `char::to_lowercase` from std;
- a **built-in Latin diacritic fold table** (Latin-1 + Latin Extended-A, a few
  hundred entries) so `José` and `Jose` share grams;
- whitespace collapsed, nothing else.

Declared as `fold = latin` (default) or `fold = none`. This is deliberately **not
a Unicode collation engine** — it takes no `unicode-normalization` dependency and
makes no promise about Turkish dotless i, Greek final sigma, or CJK segmentation
(CJK works, as substring matching, because ideographs are their own grams). The
limit is stated rather than papered over; a stronger fold is a later declaration
value, and since it folds into the identity it is a new type when it changes.

### Folding into `STRUCT_HASH`

Each declaration contributes a synthetic `#fuzzy` entry carrying the field
name(s), `n`, and the fold profile — all three reach the stored posting bytes.
Declaration **order** folds too, as with lists (RFCs 0051/0052), because it is
the index's position in the `Pivot`'s root vector. Changing any of it yields a
new type; the engine does no migration (RFC 0040).

## Alternatives

| | Read | Write | Why it lost |
|---|---|---|---|
| **n-gram postings** | T prefix scans + verify | `L+n-1` keys | *chosen* — **is** a `BpTree<SecKey>` |
| BK-tree | pointer chase, random reads | rebalance is global | IOPS are the scarce resource, and the tree's shape depends on insertion order |
| FST + Levenshtein automaton | the fastest read there is | batch rebuild | an immutable batch-built structure against an engine whose unit is one incremental atomic batch |
| SymSpell (deletion neighbourhood) | exact-match hit | `C(L,k)` keys — ~200 for L=20, k=2 | an order of magnitude worse on write to remove a verify step that the count filter already makes cheap |
| Phonetic (soundex/metaphone) | one exact hit | one key | nearly free, but answers a different question ("sounds like", not "is close to") and is language-specific — a plausible *additional* kind, not this one |
| No index; scan and score | O(collection) | nothing | the baseline, and genuinely competitive for a small tenant — the honest bar this must clear, in the spirit of 0053 |

## Open questions

- **The count filter's exact constant.** The `T - n*k` form above is the
  conservative one; tighter variants exist and the padding scheme shifts it. It
  must be pinned by property tests against a brute-force Levenshtein over
  generated strings — a filter that is wrong by one silently loses matches, and
  nothing else in the pipeline would notice.
- **Common grams.** A gram present in most of the collection makes its posting
  run the collection. Capping the scan per gram bounds the read but forfeits
  exactness (it can only be best-effort from there), so it would have to be
  declared, not inferred. Deferred until a measured workload shows the grain
  argument failing.
- **Its own lane?** Posting keys are ~23 bytes and extremely uniform — they would
  compress far better under a dictionary of their own than beside record bodies.
  This is precisely the reasoning that split `Lane::Recency` out of
  `Lane::Records` (RFC 0054): an id-sized entry and a whole record are different
  content, and one per-type dictionary can only model one of them well. A
  `Lane::Fuzzy` is the likely answer.
- **Composite fuzzy.** `#[wavedb::fuzzy((first, last))]` — concatenate the fields
  into one indexed string, or index them separately and union? Concatenation is
  simpler and matches how `#[wavedb::list((a, b))]` reads; whether it is what a
  name search wants is a different question.
- **The wire.** Like `search_by` and `listed`, this refuses over the client
  transport until it has a `Command` of its own. A fuzzy search is *exactly* the
  read an interactive client wants, so this one is not optional the way the
  others were.
