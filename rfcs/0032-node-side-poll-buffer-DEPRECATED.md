# RFC 0032 — Node-side stateful poll buffer — DEPRECATED

- **Status:** Deprecated — superseded by
  [RFC 0022 — Live sync by navigation catch-up](0022-live-sync-navigation-catchup.md)
- **Was:** the interim HTTP-poll mechanism (W5.5, landed 2026-07-16, then removed W6)

## What it proposed

For clients watching over plain HTTP (no WebSocket to push down), the node kept a
**per-session buffer** of events: `quick-node::poll::PollTable`, keyed by
`(tenant, token-session)`, capacity 1024 drop-oldest, idle sessions pruned after
a TTL (~1 min) by a maintenance task. A poll tick drained the caller's buffer.
The client re-declared its full topic list each tick and the node *replaced* the
session's subscription set.

## Why it was replaced

The buffer is **per-session node state**, and that state is the problem:

1. **It can overflow.** Cap 1024 drop-oldest means a slow or long-absent client
   silently loses events.
2. **It expires.** The TTL prunes idle sessions — a client away longer than the
   TTL comes back to an empty buffer and a silent gap.
3. **It is lost on restart.** A node restart drops every buffer; events during
   the downtime are gone.

All three are "the node had to remember, and forgot."

## What replaced it

[RFC 0022](0022-live-sync-navigation-catchup.md) made the poll path **stateless**
(W6): the `PollTable` / TTL pruning was **deleted**. Each declared topic now
carries an **instant cursor**, and the node answers by navigating the disk
(`Changes` per topic through the registry) — it holds *zero* poll state. The
**client** owns the cursors, so they survive an outage and a node restart loses
nothing; the pre-W6 "events during downtime are missed" gap is closed for the
poll path.
