//! Declared subscriptions and the [`NotifyStore`] that feeds them — the
//! node half of M7 live sync.
//!
//! A WebSocket connection [`Subscribe`](wavedb_net::ws::ClientMsg::Subscribe)s
//! to a [`Topic`] (a Unique anchor or one collection `Pivot`). Every
//! committed mutation crosses [`Store::note_mutation`]; [`NotifyStore`]
//! wraps the engine store, overrides that hook, and routes the event to
//! exactly the connections subscribed to its `(tenant, topic)` key —
//! O(subscribers), an exact-match lookup, never a scan of the data. No
//! `dyn`: the wrapper is one concrete type the node's serve path names.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tokio::sync::mpsc::UnboundedSender;
use wavedb_core::notify::{Mutation, MutationKind};
use wavedb_core::{Id, Store, U48, Write};
use wavedb_net::ws::{EventKind, RecordEvent, ServerMsg, Topic};

/// The routing key: a topic scoped to the tenant whose data it is. The
/// tenant is the subscriber connection's bound identity, never a client
/// field — a connection can only ever watch its own tenant.
type Key = (U48, Topic);

/// One connection's interest in a topic: its id (to unregister on
/// disconnect) and the channel its session task drains.
struct Sub {
    conn: u64,
    tx: UnboundedSender<ServerMsg>,
}

/// The live subscription table — one per node, shared (`Rc<RefCell>`) by
/// every connection task on the serve `LocalSet` (current-thread, so no
/// lock).
#[derive(Default)]
pub struct SubTable {
    by_topic: HashMap<Key, Vec<Sub>>,
    /// Monotonic connection id source — each WebSocket session takes one to
    /// tag (and later drop) its subscriptions.
    next_conn: u64,
}

impl SubTable {
    /// Mint a fresh connection id for a session.
    pub const fn new_conn(&mut self) -> u64 {
        let conn = self.next_conn;
        self.next_conn += 1;
        conn
    }

    /// Register `conn`'s interest in `topic` under `tenant`. The session
    /// guarantees one call per `(conn, topic)` (it tracks its own set).
    pub fn subscribe(
        &mut self,
        tenant: U48,
        topic: Topic,
        conn: u64,
        tx: UnboundedSender<ServerMsg>,
    ) {
        self.by_topic
            .entry((tenant, topic))
            .or_default()
            .push(Sub { conn, tx });
    }

    /// Drop `conn`'s interest in `topic`.
    pub fn unsubscribe(&mut self, tenant: U48, topic: Topic, conn: u64) {
        let key = (tenant, topic);
        if let Some(subs) = self.by_topic.get_mut(&key) {
            subs.retain(|s| s.conn != conn);
            if subs.is_empty() {
                self.by_topic.remove(&key);
            }
        }
    }

    /// Push one mutation to the connections watching its topic. A closed
    /// receiver (its session ended between `apply` and here) is pruned.
    fn publish(&mut self, m: &Mutation) {
        let topic = Topic {
            struct_hash: m.struct_hash,
            pivot: m.pivot,
        };
        let key = (m.tenant, topic);
        let Some(subs) = self.by_topic.get_mut(&key) else {
            return;
        };
        let event = ServerMsg::Event(RecordEvent {
            topic,
            id: m.id,
            kind: match m.kind {
                MutationKind::Saved => EventKind::Saved,
                MutationKind::Removed => EventKind::Removed,
            },
            body: m.body.clone(),
        });
        subs.retain(|s| s.tx.send(event.clone()).is_ok());
        if subs.is_empty() {
            self.by_topic.remove(&key);
        }
    }
}

/// A shared handle to the node's subscription table — one per node, cloned
/// into the [`NotifyStore`] publisher and every WebSocket session.
pub type Subscriptions = Rc<RefCell<SubTable>>;

/// A shared handle to the node's poll-session buffers — the HTTP half of
/// the same publish fan-out.
pub type Polls = Rc<RefCell<crate::poll::PollTable>>;

/// The engine store wrapped so committed mutations reach subscribers.
///
/// Forwards every [`Store`] read/write to the inner engine (a shared
/// `Rc<PageStore>` — the node's maintenance loop and final commit drive the
/// same engine directly) and overrides
/// [`note_mutation`](Store::note_mutation) to publish — to the WebSocket
/// subscribers (pushed now) and to the poll-session buffers (drained on
/// the client's next tick). The node serves **through** this wrapper, so a
/// mutation driven by any transport — a POST command, a WebSocket `Call`,
/// or a `#[server]` body — reaches the live watchers; node-side seeding
/// (which touches the raw engine before serving) does not, and neither do
/// cache mirrors (a different store).
pub struct NotifyStore<S> {
    inner: Rc<S>,
    subs: Subscriptions,
    polls: Polls,
}

impl<S> NotifyStore<S> {
    /// Wrap a shared engine handle, publishing mutations into `subs` (the
    /// WebSocket push) and `polls` (the HTTP buffers).
    pub const fn new(inner: Rc<S>, subs: Subscriptions, polls: Polls) -> Self {
        Self { inner, subs, polls }
    }
}

impl<S: Store> Store for NotifyStore<S> {
    async fn get(&self, id: Id) -> wavedb_core::Result<Option<Vec<u8>>> {
        self.inner.get(id).await
    }

    async fn get_of(
        &self,
        struct_hash: u64,
        id: Id,
    ) -> wavedb_core::Result<Option<Vec<u8>>> {
        self.inner.get_of(struct_hash, id).await
    }

    async fn apply(&self, batch: &[Write]) -> wavedb_core::Result<()> {
        self.inner.apply(batch).await
    }

    fn note_mutation(&self, mutation: impl FnOnce() -> Mutation) {
        // Building the event costs a body clone; only the node pays it (an
        // ordinary store's default drops the closure unbuilt), and only on
        // writes. Both publishes are exact-match routing — no data scan.
        let mutation = mutation();
        self.subs.borrow_mut().publish(&mutation);
        self.polls.borrow_mut().publish(&mutation);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use tokio::sync::mpsc::unbounded_channel;
    use wavedb_core::notify::{Mutation, MutationKind};
    use wavedb_core::{Id, LocalId, Store, U48, Write};
    use wavedb_net::ws::{EventKind, ServerMsg, Topic};

    use super::{NotifyStore, SubTable};

    /// A store that records nothing — the inner engine stand-in.
    #[derive(Default)]
    struct NullStore;
    impl Store for NullStore {
        async fn get(&self, _: Id) -> wavedb_core::Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn apply(&self, _: &[Write]) -> wavedb_core::Result<()> {
            Ok(())
        }
    }

    fn mutation(tenant: U48, pivot: Option<LocalId>) -> Mutation {
        Mutation {
            struct_hash: 0xABCD,
            tenant,
            pivot,
            id: Id::new(7, tenant, false, 1),
            kind: MutationKind::Saved,
            body: vec![1, 2, 3],
        }
    }

    fn table() -> Rc<RefCell<SubTable>> {
        Rc::new(RefCell::new(SubTable::default()))
    }

    fn polls() -> super::Polls {
        Rc::new(RefCell::new(crate::poll::PollTable::default()))
    }

    fn unique_topic() -> Topic {
        Topic {
            struct_hash: 0xABCD,
            pivot: None,
        }
    }

    #[tokio::test]
    async fn a_mutation_reaches_only_its_topics_subscribers() {
        let tenant = U48::from(3u32);
        let subs = table();
        let (tx, mut rx) = unbounded_channel();
        let topic = unique_topic();
        let conn = subs.borrow_mut().new_conn();
        subs.borrow_mut().subscribe(tenant, topic, conn, tx);

        // A subscriber on a *different* pivot must not hear this.
        let (other_tx, mut other_rx) = unbounded_channel();
        let other = subs.borrow_mut().new_conn();
        subs.borrow_mut().subscribe(
            tenant,
            Topic {
                struct_hash: 0xABCD,
                pivot: Some(LocalId::new(9, true, 0)),
            },
            other,
            other_tx,
        );

        let store = NotifyStore::new(Rc::new(NullStore), subs, polls());
        store.note_mutation(|| mutation(tenant, None));

        let ServerMsg::Event(event) = rx.recv().await.unwrap() else {
            panic!("expected an event");
        };
        assert_eq!(event.topic, topic);
        assert_eq!(event.kind, EventKind::Saved);
        assert_eq!(event.body, vec![1, 2, 3]);
        assert!(other_rx.try_recv().is_err(), "wrong topic must stay silent");
    }

    #[tokio::test]
    async fn another_tenants_mutation_is_never_delivered() {
        let subs = table();
        let (tx, mut rx) = unbounded_channel();
        let conn = subs.borrow_mut().new_conn();
        subs.borrow_mut()
            .subscribe(U48::from(3u32), unique_topic(), conn, tx);
        let store = NotifyStore::new(Rc::new(NullStore), subs, polls());
        // Same topic shape, foreign tenant — the key differs.
        store.note_mutation(|| mutation(U48::from(4u32), None));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn unsubscribe_stops_delivery_and_empties_the_table() {
        let tenant = U48::from(5u32);
        let topic = unique_topic();
        let subs = table();
        let (tx, mut rx) = unbounded_channel();
        let conn = subs.borrow_mut().new_conn();
        subs.borrow_mut().subscribe(tenant, topic, conn, tx);
        subs.borrow_mut().unsubscribe(tenant, topic, conn);

        let store =
            NotifyStore::new(Rc::new(NullStore), Rc::clone(&subs), polls());
        store.note_mutation(|| mutation(tenant, None));
        assert!(rx.try_recv().is_err(), "unsubscribed conn hears nothing");
        assert!(subs.borrow().by_topic.is_empty());
    }

    #[tokio::test]
    async fn a_closed_receiver_is_pruned_on_publish() {
        let tenant = U48::from(6u32);
        let subs = table();
        let (tx, rx) = unbounded_channel();
        let conn = subs.borrow_mut().new_conn();
        subs.borrow_mut()
            .subscribe(tenant, unique_topic(), conn, tx);
        drop(rx); // the session task ended without unregistering.

        let store =
            NotifyStore::new(Rc::new(NullStore), Rc::clone(&subs), polls());
        store.note_mutation(|| mutation(tenant, None));
        assert!(
            subs.borrow().by_topic.is_empty(),
            "the dead subscription is dropped"
        );
    }
}
