//! Per-session event buffers — the node half of the HTTP-poll watch
//! (`wavedb_net::sync`).
//!
//! A POST-only watcher has no connection to push down, so the node buffers
//! its subscriptions' events keyed by `(tenant, session)` — the session id
//! comes from the verified access token, never a client field — and each
//! "anything new?" poll **replaces** the session's declared topic set and
//! drains the buffer. Replace semantics make registration stateless: a
//! node restart heals on the client's next tick, a dropped topic stops
//! buffering, nothing incremental to reconcile.
//!
//! Buffers are bounded ([`EVENT_CAP`], drop-oldest — a stalled poller must
//! not grow the node; missed events are the honest pre-W6 gap the journal
//! cursor closes) and sessions that stop polling age out (the maintenance
//! loop calls [`PollTable::prune`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use wavedb_core::U48;
use wavedb_core::notify::{Mutation, MutationKind};
use wavedb_net::sync::SyncRequest;
use wavedb_net::ws::{EventKind, RecordEvent, Topic};

/// Most events one session buffers between polls; beyond it the oldest
/// drop first.
const EVENT_CAP: usize = 1024;

/// One polling session's key: the tenant is the verified identity, the
/// session id the token's claim.
type Key = (U48, u128);

/// One polling session: its declared topics and the events awaiting its
/// next drain.
struct PollBox {
    topics: HashSet<Topic>,
    events: VecDeque<RecordEvent>,
    last_poll: Instant,
}

/// The node's poll-session table — shared (`Rc<RefCell>`) like the
/// WebSocket [`SubTable`](crate::subscribe::SubTable) it mirrors.
#[derive(Default)]
pub struct PollTable {
    boxes: HashMap<Key, PollBox>,
    /// Exact-match publish routing: which sessions watch a topic.
    index: HashMap<(U48, Topic), HashSet<u128>>,
}

impl PollTable {
    /// One poll: replace the session's declared topics with the request's
    /// list, then drain everything buffered since the last drain.
    pub fn sync(
        &mut self,
        tenant: U48,
        session: u128,
        request: &SyncRequest,
    ) -> Vec<RecordEvent> {
        let declared: HashSet<Topic> =
            request.subscribe.iter().copied().collect();
        let entry = self.boxes.entry((tenant, session)).or_insert(PollBox {
            topics: HashSet::new(),
            events: VecDeque::new(),
            last_poll: Instant::now(),
        });
        entry.last_poll = Instant::now();
        let previous = std::mem::replace(&mut entry.topics, declared.clone());
        let drained = entry.events.drain(..).collect();
        for dropped in previous.difference(&declared) {
            unindex(&mut self.index, tenant, *dropped, session);
        }
        for added in declared.difference(&previous) {
            self.index
                .entry((tenant, *added))
                .or_default()
                .insert(session);
        }
        drained
    }

    /// Buffer one committed mutation for every session watching its topic
    /// (exact match, never a scan). Over [`EVENT_CAP`] the oldest drop.
    pub fn publish(&mut self, m: &Mutation) {
        let topic = Topic {
            struct_hash: m.struct_hash,
            pivot: m.pivot,
        };
        let Some(sessions) = self.index.get(&(m.tenant, topic)) else {
            return;
        };
        let event = RecordEvent {
            topic,
            id: m.id,
            kind: match m.kind {
                MutationKind::Saved => EventKind::Saved,
                MutationKind::Removed => EventKind::Removed,
            },
            body: m.body.clone(),
        };
        for session in sessions {
            if let Some(entry) = self.boxes.get_mut(&(m.tenant, *session)) {
                if entry.events.len() >= EVENT_CAP {
                    entry.events.pop_front();
                }
                entry.events.push_back(event.clone());
            }
        }
    }

    /// Drop sessions that have not polled within `ttl` — their client is
    /// gone (or will re-register in full on its next tick anyway).
    pub fn prune(&mut self, ttl: Duration) {
        let now = Instant::now();
        let dead: Vec<Key> = self
            .boxes
            .iter()
            .filter(|(_, b)| now.duration_since(b.last_poll) > ttl)
            .map(|(k, _)| *k)
            .collect();
        for key in dead {
            if let Some(entry) = self.boxes.remove(&key) {
                for topic in entry.topics {
                    unindex(&mut self.index, key.0, topic, key.1);
                }
            }
        }
    }
}

/// Remove one session from a topic's publish route.
fn unindex(
    index: &mut HashMap<(U48, Topic), HashSet<u128>>,
    tenant: U48,
    topic: Topic,
    session: u128,
) {
    if let Some(sessions) = index.get_mut(&(tenant, topic)) {
        sessions.remove(&session);
        if sessions.is_empty() {
            index.remove(&(tenant, topic));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wavedb_core::notify::{Mutation, MutationKind};
    use wavedb_core::{Id, U48};
    use wavedb_net::sync::SyncRequest;
    use wavedb_net::ws::Topic;

    use super::{EVENT_CAP, PollTable};

    const TENANT: u32 = 4;

    fn topic() -> Topic {
        Topic {
            struct_hash: 0xABCD,
            pivot: None,
        }
    }

    fn mutation(n: u64) -> Mutation {
        Mutation {
            struct_hash: 0xABCD,
            tenant: U48::from(TENANT),
            pivot: None,
            id: Id::new(n, U48::from(TENANT), false, 0),
            kind: MutationKind::Saved,
            body: vec![7],
        }
    }

    fn declare(topics: Vec<Topic>) -> SyncRequest {
        SyncRequest { subscribe: topics }
    }

    #[test]
    fn declared_topics_buffer_and_drain_in_order() {
        let mut table = PollTable::default();
        let tenant = U48::from(TENANT);
        assert!(table.sync(tenant, 1, &declare(vec![topic()])).is_empty());
        table.publish(&mutation(1));
        table.publish(&mutation(2));
        let drained = table.sync(tenant, 1, &declare(vec![topic()]));
        assert_eq!(
            drained.iter().map(|e| e.id.key()).collect::<Vec<_>>(),
            vec![1, 2],
            "commit order, oldest first"
        );
        assert!(
            table.sync(tenant, 1, &declare(vec![topic()])).is_empty(),
            "a drain empties the buffer"
        );
    }

    #[test]
    fn replace_semantics_stop_buffering_dropped_topics() {
        let mut table = PollTable::default();
        let tenant = U48::from(TENANT);
        table.sync(tenant, 1, &declare(vec![topic()]));
        // The next poll declares nothing — the topic is dropped.
        table.sync(tenant, 1, &declare(Vec::new()));
        table.publish(&mutation(1));
        assert!(table.sync(tenant, 1, &declare(Vec::new())).is_empty());
    }

    #[test]
    fn sessions_and_tenants_are_isolated() {
        let mut table = PollTable::default();
        let tenant = U48::from(TENANT);
        table.sync(tenant, 1, &declare(vec![topic()]));
        table.sync(tenant, 2, &declare(vec![topic()]));
        table.sync(U48::from(9u32), 3, &declare(vec![topic()]));
        table.publish(&mutation(1));
        assert_eq!(table.sync(tenant, 1, &declare(vec![topic()])).len(), 1);
        assert_eq!(
            table.sync(tenant, 2, &declare(vec![topic()])).len(),
            1,
            "both sessions of the tenant hear it"
        );
        assert!(
            table
                .sync(U48::from(9u32), 3, &declare(vec![topic()]))
                .is_empty(),
            "a foreign tenant never does"
        );
    }

    #[test]
    fn a_stalled_session_caps_its_buffer_dropping_oldest() {
        let mut table = PollTable::default();
        let tenant = U48::from(TENANT);
        table.sync(tenant, 1, &declare(vec![topic()]));
        for n in 0..=u64::try_from(EVENT_CAP).unwrap() {
            table.publish(&mutation(n));
        }
        let drained = table.sync(tenant, 1, &declare(vec![topic()]));
        assert_eq!(drained.len(), EVENT_CAP);
        assert_eq!(drained[0].id.key(), 1, "the oldest event was dropped");
    }

    #[test]
    fn prune_forgets_sessions_that_stopped_polling() {
        let mut table = PollTable::default();
        let tenant = U48::from(TENANT);
        table.sync(tenant, 1, &declare(vec![topic()]));
        table.prune(Duration::from_hours(1));
        table.publish(&mutation(1));
        assert_eq!(
            table.sync(tenant, 1, &declare(vec![topic()])).len(),
            1,
            "a fresh session survives the prune"
        );
        table.prune(Duration::ZERO);
        table.publish(&mutation(2));
        assert!(
            table.sync(tenant, 1, &declare(vec![topic()])).is_empty(),
            "a pruned session re-registers empty (and missed the event)"
        );
    }
}
