# RFC 0010 — Metadata and record envelopes

- **Status:** Implemented
- **Crates:** `wavedb-core`
- **Code:** `metadata.rs` (`crates/wavedb-core/src/`); reference
  `docs/wire_format.md` §"Engine record layout"

## Summary

Stored values are **`STRUCT_HASH`-headed**: every value opens with an 8-byte LE
`STRUCT_HASH`, so storage routes on it and decode verifies it — a stale or
foreign `Id` can never decode as the wrong type. Three envelope forms sit on top
of the [WaveWire codec](0003-wavewire-wire-format.md), and user records carry a
fixed **`Metadata`** header that makes every version chainable.

## Motivation

Two needs meet here: (1) type-safety at the byte boundary — the head verifies
the type before any field is read; (2) a per-version header that records the
chain link, authorship, and permission without polluting the domain type
([RFC 0009](0009-anchors-succession-and-history.md)).

## Design — the three envelopes

- **bare** (`Pivot` records — pure addressing, no history):
  `[STRUCT_HASH (8)][WaveWire bytes]`.
- **record** (Unique + NonUnique user data):
  `[STRUCT_HASH (8)][meta_len (u32 LE)][WaveWire(Metadata)][WaveWire body]` — the
  `meta_len` prefix splits two independently-decodable payloads.
- **B+tree node**: `[BPTREE_NODE_HASH (8)][kind (u8)][WaveWire bytes]`.

## Design — `Metadata` (26-byte stack)

| Field | Type | Stack | Heap (when `Some`) |
|-------|------|-------|--------------------|
| `previous` | `Option<u64>` | 1 | 8 (predecessor's instant) |
| `succession` | `Succession` | 9 | — |
| `pivot_id` | `Option<LocalId>` | 1 | 10 (owning Pivot back-link) |
| `user` | `U48` | 6 | — |
| `device_created` | `u64` | 8 | — |
| `permission` | `Option<PermissionRef>` | 1 | variable |

- **`Succession` is hand-encoded**, not the derive's enum form: a fixed 9-byte
  stack (`tag (1) + instant (8 LE)`). The payload never varies, so the derive's
  `u32` length prefix would be dead weight on every stored record. A Unique first
  version (every `Option` `None`) is the minimal case: 26 stack bytes, zero heap.
- **`pivot_id` is a back-link.** A NonUnique record carries its owning `Pivot`'s
  `LocalId`, so `save` can reach every tree root and reindex *without* the
  collection handle ([RFC 0011](0011-bptree-index-and-collections.md)).
- **`user` / `device_created` / `permission`** are the "who / when / under which
  rule" the chain reviews — set from the verified caller
  ([RFC 0026](0026-auth-tokens.md)), never client-authored blindly.

## Consequences

- **Decode verifies the head** on every read; a wrong hash is a typed error, not
  a misparse.
- **Metadata rides the wire** on sync frames (`Change::Saved`, `RecordEvent`)
  so a cache mirrors the node's chain data *verbatim* at the node's own derived
  slots — the mirror's bytes are identical to the node's
  ([RFC 0022](0022-live-sync-navigation-catchup.md)). Plain `Get`/`Save` replies
  are body-only; cursors only ever come from meta-carrying frames.
