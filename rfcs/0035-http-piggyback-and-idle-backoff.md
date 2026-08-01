# RFC 0035 — W7: HTTP piggyback + idle backoff

- **Status:** Implemented (2026-07-24)
- **Crates:** `wavedb-net`, `wavedb-quick-node`
- **Depends on:** [RFC 0022](0022-live-sync-navigation-catchup.md),
  [RFC 0020](0020-net-transport-dumb-tunnel.md),
  [RFC 0021](0021-connection-manager.md)

> **Progress.**
> - **Idle backoff — landed (2026-07-24).** `manager::poll` now carries an
>   adaptive `interval` (base = the caller's `HttpPoll` duration): an empty tick
>   multiplies it by `IDLE_GROWTH` (2×), capped at `MAX_IDLE_FACTOR` (16×) the
>   base; a non-empty answer or a new subscription snaps it back to the base.
>   The loop woke on `Wake::{Closed, Cmd, Tick}`; the tick path calls
>   `next_interval` after each sync. Pure-logic tests cover snap-back, geometric
>   growth, and the ceiling; the poll e2e (`live_watch_poll_e2e.rs`) still green.
> - **Piggyback — landed (2026-07-24).** Both halves are in.
>   - *Wire + node.* `Request` carries `sync: Vec<TopicCursor>` (empty = no
>     piggyback); a new `StreamFrame::Sync(Vec<u8>)` **leads** the response.
>     `dispatch::handle` returns `Answer { response, sync }` — after the command
>     it runs the same `Changes` navigation as the poll path (extracted into a
>     shared `navigate`, reused by `sync_poll` and a new `piggyback`) and the
>     HTTP writer emits the leading `Sync` frame; the delta is best-effort (a
>     refused navigation leaves the command reply intact).
>   - *Client wiring (manager-orchestrated).* `manager::post` now takes
>     `(addr, auth, frame)` and assembles the `Request` itself. When a poll
>     watch of that identity is live, `run_post` snapshots its cursors
>     (`PollCmd::Snapshot`) into the declaration, and peels the **leading**
>     `Sync` frame back to the actor (`PollCmd::ApplyDelta`) — so only the first
>     response frame is ever inspected. The poll actor gained a shared
>     `apply_reply` (fan-out + advance cursors) with **dedup** by cursor, so the
>     piggyback and the next ordinary tick coalesce exactly-once (the same
>     discipline as the WS overlap). Proven end-to-end by
>     `a_command_piggybacks_the_delta_to_a_live_poll_watch` (a live poll watch
>     receives an event a command carried, with no dedicated poll), plus the
>     node-side `a_command_piggybacks_the_sync_delta_for_declared_topics` and
>     `Request`/`StreamFrame::Sync` wire roundtrips. The `live_watch_poll_e2e`
>     two-process e2e stays green (and now delivers faster — commands carry the
>     changes). Whole workspace green.

## Summary

For POST-only clients that watch over HTTP polling, two refinements: **ride sync
deltas back on ordinary responses** (so a client already talking to the node
needs fewer dedicated "anything new?" polls), and **back the poll timer off when
idle** (so a quiet watch stops hammering the node every interval).

## Motivation

The stateless poll path ([RFC 0022](0022-live-sync-navigation-catchup.md)) works,
but it is wasteful in two shapes: (1) an app that is *already* POSTing commands
still fires separate poll requests on the side — the changes could have ridden
the reply it was already getting; (2) an idle screen polls at the fixed interval
forever even when nothing ever changes. Both are pure efficiency — the exit
already holds ([RFC 0022](0022-live-sync-navigation-catchup.md)); W7's remainder
just stops paying for it twice.

## Design (target)

- **Piggyback.** An ordinary command reply can carry the sync delta for the
  caller's declared topics — the node runs the same `Changes` navigation
  ([RFC 0022](0022-live-sync-navigation-catchup.md)) against the caller's cursors
  and attaches the result to the response frames, so a POST that was going to
  happen anyway *is* the poll. Stays stateless: cursors ride the request, the
  node holds nothing.
- **Idle backoff.** The poll actor ([RFC 0021](0021-connection-manager.md),
  `manager::poll`) lengthens its interval after consecutive empty ticks and snaps
  back to the base interval on the first non-empty answer (or a new subscription).

## Non-goals / notes

- Ordering and coalescing semantics are unchanged from
  [RFC 0022](0022-live-sync-navigation-catchup.md) (a tick delivers each changed
  record once at its live state).
- This does **not** touch the WebSocket path — pushed watches already deliver on
  commit; the reconnect gap there is [RFC 0034](0034-ws-reconnect-catchup.md).
