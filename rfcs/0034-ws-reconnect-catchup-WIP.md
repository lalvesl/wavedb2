# RFC 0034 — W6: WebSocket reconnect catch-up

- **Status:** In progress (WIP) — the active work item; plan in this file
- **Crates:** `wavedb-net` (primary), `wavedb-quick-node`, `wavedb`
- **Code (to touch):** `net::manager::ws_conn`, `net::manager::poll`,
  `net::ws` (`TopicOk`), `quick-node::serve_ws`, `examples/*/tests/live_watch_e2e.rs`
- **Depends on:** [RFC 0021](0021-connection-manager.md),
  [RFC 0022](0022-live-sync-navigation-catchup.md)

## Summary

Close the downtime gap on the **push** path so it matches what the poll path
already does ([RFC 0022](0022-live-sync-navigation-catchup.md)): a WebSocket
watch that loses its socket must **survive**, reconnect, replay everything
committed while it was down (by navigation catch-up), and resume live push —
without ending the watcher's stream.

## Motivation — the gap is bigger than "add a cursor"

Today WS watches **do not reconnect at all**. When the socket ends
(`ws_conn.rs`, `msgs.next()` → `None`) the actor breaks, closes, and drops its
`topics` map — dropping every watcher channel, so each
`UniqueWatch/CollectionWatch::next()` returns `Ok(None)` and the stream just
ends. The manager only respawns on a *new* `watch()` call. So a transient
network blip silently terminates every live watch for that identity. W6-WS =
**reconnect + per-topic cursors + navigation catch-up**, all inside the
`ws_conn` actor.

The machinery to catch up already exists and is transport-generic:
`core::expose_changes` (`collection_changes` / `unique_changes`),
`Command::Changes`, and the stateless `SyncRequest`/`SyncReply` exchange the poll
actor drives. Every pushed `RecordEvent` already carries its instant —
`EventKind::Removed(u64)` directly, `EventKind::Saved` via
`meta.succession = CreatedAt(instant)` (the node populates `meta` on saves).

## Design

### 1. Reconnect lives inside the `ws_conn` actor

Not the manager loop — the manager does not hold the watcher channels (they live
in `TopicState.subs`). Reconnecting in-actor keeps those channels **alive across
a blip**, which is the point (a watch survives a disconnect). The manager's
lifecycle authority is unchanged: it still owns register/unregister; the actor
loops instead of exiting on socket loss, exiting only when its `cmds` channel
closes (last watcher gone) or on a fatal refusal.

### 2. Per-topic cursor, advanced by every delivered event

`TopicState` gains `cursor: Option<u64>`, advanced (`max`) on every fan-out via a
small `event_instant(&RecordEvent)` helper (`Removed(i) → i`;
`Saved → meta CreatedAt`). Because instants are strictly monotone per topic
([RFC 0009](0009-anchors-succession-and-history.md)) and events arrive in commit
order, the cursor only ever moves forward.

### 3. Catch-up reuses the HTTP `Changes` exchange

On reconnect, issue one `SyncRequest` (all resubscribed topics + their cursors)
→ `SyncReply` (events + advanced cursors), exactly as the poll actor does — fan
the returned events out (advancing cursors) before resuming live push. The
`SyncRequest`-build + POST + decode should be **extracted** from `poll.rs` into a
shared `pub(crate)` helper both paths call. The watch connection stays
push-only; catch-up is a side POST.

### 4. Seed the cursor at subscribe (recommended — closes the zero-event gap)

The plain "cursor = last delivered event's instant" leaves a hole: a topic that
received **zero** events before a disconnect has no cursor, so reconnect catch-up
with `since: None` registers at the new tail and **skips** downtime events.
Recommended fix: add the topic's current tail instant to the subscribe ack —
`ServerMsg::TopicOk(Topic)` → `TopicOk(Topic, u64)` (the node already computes it
via `Changes(None)` registration). The client seeds `state.cursor` on the ack.
*Lighter alternative (no wire change):* after a topic first goes live, fire one
`sync_once([(topic, None)])` to seed — one extra POST per topic.

### 5. Dedup by cursor-gating live fan-out

After catch-up, resumed push may re-deliver the overlap window. Drop any live
event with `instant ≤ topic.cursor`. Monotone instants make this exactly-once in
steady state (the gate is a no-op when no reconnect happened) — consistent with
the existing "poll coalesces, WS delivers every mutation" note: a reconnect is
the one place the WS path coalesces, which is honest.

### 6. Fatal vs transient

Mirror `poll.rs`: a transport fault on re-dial → bounded backoff + retry
(`platform::time::sleep`, works both targets); a node refusal on re-`Hello`
(expired/forged identity) → fatal, end the watches. The *first* `open()` still
fails fast to `watch()` (a client wants to know its initial subscribe failed);
only drops after establishment reconnect.

## Behaviour change (call out)

Watch streams now **survive transient disconnects** (they used to end). This
redefines `next() → Ok(None)` to mean *permanently ended* (fatal refusal or last
guard dropped), not *the socket blipped*. Document in `watch.rs`.

## File budget

`ws_conn.rs` is at ~223 non-test lines; reconnect + catch-up + cursor tracking
blows past the 350 budget ([RFC 0002 §6](0002-architectural-hard-rules.md)) —
plan the split up front: actor loop + handlers stay; dial/reconnect/backoff move
to `manager/ws_dial.rs`; catch-up uses the shared Step-3 helper (which also
shrinks `poll.rs`).

## Testing

- **Net unit** (loopback mini-node, the pattern in `manager/mod.rs` tests):
  (a) server drops the socket after events → client re-dials, re-subscribes,
  stream **stays open**; (b) "downtime" events delivered via reconnect catch-up →
  received once, in order; (c) fatal refusal on re-Hello → streams end;
  (d) zero-event-then-downtime → caught up (guards §4).
- **E2E exit proof:** extend `live_watch_e2e.rs` (two processes) with a node
  restart mid-watch — the watch survives and catches up. Mirror
  `live_watch_poll_e2e.rs`, which already proves this for the poll path.
- **Node unit:** `TopicOk` carries the tail; subscribing to a non-empty
  collection acks the current tail.

## Alternatives

- **Reconnect in the manager loop** (respawn + transfer watcher set): rejected —
  the manager does not hold the watcher channels, so a respawn would end the
  streams, defeating the purpose.
- **Catch-up over the WS `Call` channel** instead of a side POST: rejected —
  the watch connection would have to handle `Item`/`End` interleaved with events;
  reusing the tested stateless HTTP `Changes` exchange is simpler and shares code
  with the poll path.
