# RFC 0029 — Bloom-filter screen-sync — DEPRECATED

- **Status:** Deprecated — rejected in favour of
  [RFC 0022 — Live sync by navigation catch-up](0022-live-sync-navigation-catchup.md)
- **Was:** an early "what is this screen showing?" sync idea

## What it proposed

A client would summarise the set of records currently on a screen as a **Bloom
filter** and send it to the node; the node would answer with anything that
changed relative to the filter — a compact "here is my screen state, tell me the
diff" without enumerating ids.

## Why it was rejected

Rejected 2026-07-10, before it was built:

1. **It forces a full-dataset scan.** To answer a filter, the node must test its
   *whole* dataset for the tenant/type against it — the opposite of the
   one-IO-per-read goal ([RFC 0001](0001-vision-and-non-goals.md)).
2. **False positives are inherent.** A Bloom filter can only say "probably
   present," so the diff is approximate — wrong for a sync layer that must be
   exact.
3. **Exact subscriptions already give live filtering.** A topic *is* a precise
   subscription (a Unique anchor or one collection Pivot), so the node pushes
   exactly the matching mutations with no membership test and no false positives.

## What replaced it

[RFC 0022](0022-live-sync-navigation-catchup.md): **declared subscriptions** for
the live stream (exact topic match, O(subscribers-of-this-topic) fan-out) and
**navigation catch-up** past an instant cursor for reconnect — both exact, both
without scanning the dataset.
