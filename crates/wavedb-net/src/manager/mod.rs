//! The connection manager — **one never-ending background task per
//! process** that owns every exchange with a node.
//!
//! Every request the client makes — an HTTP POST command or a WebSocket
//! watch — routes through this task; it is the single place connections
//! are dialed, shared, and torn down (and the natural seam for the M7
//! reconnect cursor and the offline write queue to land on). Natively it
//! lives on a dedicated thread with its own current-thread runtime; in the
//! browser it is a detached `spawn_local` task — no tokio in wasm
//! ([`wavedb_platform::task`] is the seam). It boots lazily on first use
//! and never ends.
//!
//! **Watches multiplex.** All watches presenting the same identity to the
//! same address share ONE WebSocket connection (`ws_conn` — the `Hello`
//! runs once, each topic subscribes once, events fan out to every watcher
//! of their topic). No pumping falls on the watchers: the connection's
//! reader pushes each event into the right watcher channel as it arrives.
//!
//! **Watches can ride plain HTTP** ([`WatchMode::HttpPoll`]): a
//! per-identity `poll` loop asks the node "anything new?" on an adjustable
//! timer and fans the buffered events out the same way — for clients whose
//! path to the node cannot hold a WebSocket open. Here the timer *is* the
//! pump.

mod actor;
mod boot;
mod poll;
mod ws_conn;

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use futures::StreamExt;
use futures::channel::{mpsc, oneshot};

use crate::error::{Error, Result};
use crate::frame::Auth;
use crate::ws::{RecordEvent, Topic};

/// How a watch's events reach the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchMode {
    /// Push over one shared WebSocket connection per identity (default).
    WebSocket,
    /// "Anything new?" POST polls at the given interval — for clients
    /// whose path to the node cannot hold a WebSocket open.
    HttpPoll(Duration),
}

/// One connection's identity key: everything on it executes as this
/// `(addr, auth)` pair — a different token is a different connection.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ConnKey {
    pub addr: String,
    pub auth: Auth,
}

/// The response frames of one POSTed exchange, pumped by the manager.
#[derive(Debug)]
pub(crate) struct PostFrames {
    rx: mpsc::UnboundedReceiver<Result<Vec<u8>>>,
}

impl PostFrames {
    /// The next `[len][bytes]` frame; `None` = the response ended cleanly.
    pub(crate) async fn next_frame(&mut self) -> Result<Option<Vec<u8>>> {
        self.rx.next().await.transpose()
    }
}

/// What the manager task routes.
enum Cmd {
    /// POST `body` to `addr`; `connected` resolves once the response head
    /// is in — with the frame channel, or the establishment fault.
    Post {
        addr: String,
        body: Vec<u8>,
        connected: oneshot::Sender<Result<PostFrames>>,
    },
    /// Register a watcher; `ack` resolves once the subscription is live.
    Watch {
        key: ConnKey,
        mode: WatchMode,
        topic: Topic,
        watch: u64,
        events: mpsc::UnboundedSender<RecordEvent>,
        ack: oneshot::Sender<Result<()>>,
    },
    /// Unregister a watcher (fired by [`WatchGuard`] on drop).
    Unwatch {
        key: ConnKey,
        poll: bool,
        topic: Topic,
        watch: u64,
    },
}

/// A cloneable handle into the manager task.
#[derive(Debug, Clone)]
struct Handle {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Handle {
    fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx
            .unbounded_send(cmd)
            .map_err(|_| Error::ManagerUnavailable)
    }
}

/// POST one request body through the manager. Resolves once the response
/// head is in, preserving the establish-vs-mid-stream error split the
/// callers rely on (a cache falls back on an establishment fault).
pub(crate) async fn post(addr: &str, body: Vec<u8>) -> Result<PostFrames> {
    let handle = boot::handle()?;
    let (connected, ready) = oneshot::channel();
    handle.send(Cmd::Post {
        addr: addr.to_owned(),
        body,
        connected,
    })?;
    ready.await.map_err(|_| Error::ManagerUnavailable)?
}

/// Ends its watch on drop — fire-and-forget unregistration; when a
/// connection's last watcher leaves, the manager closes the connection.
#[derive(Debug)]
pub struct WatchGuard {
    handle: Handle,
    key: ConnKey,
    poll: bool,
    topic: Topic,
    watch: u64,
}

impl Drop for WatchGuard {
    fn drop(&mut self) {
        let _ = self.handle.send(Cmd::Unwatch {
            key: self.key.clone(),
            poll: self.poll,
            topic: self.topic,
            watch: self.watch,
        });
    }
}

/// Watcher id source — only disambiguates senders on a shared topic.
static WATCH_IDS: AtomicU64 = AtomicU64::new(0);

/// Open a live watch on `topic`, presenting `auth` to `addr` over `mode`.
///
/// Returns once the subscription is **live** (acked over WebSocket;
/// registered by a first successful sync over HTTP poll), so a mutation
/// committed after this call cannot be missed. The channel closing means
/// the connection ended (a poll watch instead rides outages silently —
/// its loop retries every tick).
///
/// # Errors
/// A dial/handshake/poll fault, a refused identity, or
/// [`Error::ManagerUnavailable`] when the manager task cannot run.
pub async fn watch(
    addr: &str,
    auth: Auth,
    mode: WatchMode,
    topic: Topic,
) -> Result<(mpsc::UnboundedReceiver<RecordEvent>, WatchGuard)> {
    let handle = boot::handle()?;
    let key = ConnKey {
        addr: addr.to_owned(),
        auth,
    };
    let watch = WATCH_IDS.fetch_add(1, Ordering::Relaxed);
    let (events_tx, events_rx) = mpsc::unbounded();
    let (ack_tx, ack_rx) = oneshot::channel();
    handle.send(Cmd::Watch {
        key: key.clone(),
        mode,
        topic,
        watch,
        events: events_tx,
        ack: ack_tx,
    })?;
    let guard = WatchGuard {
        handle,
        key,
        poll: matches!(mode, WatchMode::HttpPoll(_)),
        topic,
        watch,
    };
    // A dropped ack = the connection died before answering; the guard
    // going down with it unregisters whatever survived.
    ack_rx
        .await
        .map_err(|_| Error::Http("watch connection closed before the ack"))??;
    Ok((events_rx, guard))
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::net::TcpListener;
    use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
    use wavedb_core::{Id, U48};
    use wavedb_platform::ws::codec::{self, Messages, Msg, OP_BINARY};
    use wavedb_wire::{from_wire, to_wire};

    use futures::StreamExt as _;

    use super::{WatchMode, watch};
    use crate::frame::Auth;
    use crate::http;
    use crate::ws::{ClientMsg, EventKind, RecordEvent, ServerMsg, Topic};

    const TOPIC_A: Topic = Topic {
        struct_hash: 0xA,
        pivot: None,
    };
    const TOPIC_B: Topic = Topic {
        struct_hash: 0xB,
        pivot: None,
    };
    /// Subscribing to this makes the server push one event on A and one on
    /// B first — a FIFO barrier: they fan out before the trigger acks.
    const TRIGGER: Topic = Topic {
        struct_hash: 0xF00D,
        pivot: None,
    };

    fn event(topic: Topic) -> RecordEvent {
        RecordEvent {
            topic,
            id: Id::new(topic.struct_hash, U48::from(1u32), false, 0),
            kind: EventKind::Saved,
            meta: None,
            body: vec![1],
        }
    }

    async fn send(w: &mut OwnedWriteHalf, msg: &ServerMsg) {
        codec::write_message(w, OP_BINARY, &to_wire(msg), false)
            .await
            .expect("server send");
    }

    /// A watch-shaped mini node: upgrade, `Hello`→`HelloOk`, ack every
    /// subscription, push the canned events on the trigger. Counts accepts.
    async fn serve_conn(
        mut msgs: Messages<OwnedReadHalf>,
        mut w: OwnedWriteHalf,
    ) {
        while let Ok(Some(msg)) = msgs.next(true).await {
            let Msg::Binary(bytes) = msg else { continue };
            match from_wire::<ClientMsg>(&bytes).expect("decode") {
                ClientMsg::Hello(_) => send(&mut w, &ServerMsg::HelloOk).await,
                ClientMsg::Subscribe(topic) => {
                    if topic == TRIGGER {
                        send(&mut w, &ServerMsg::Event(event(TOPIC_A))).await;
                        send(&mut w, &ServerMsg::Event(event(TOPIC_B))).await;
                    }
                    send(&mut w, &ServerMsg::TopicOk(topic)).await;
                }
                ClientMsg::Unsubscribe(topic) => {
                    send(&mut w, &ServerMsg::TopicOk(topic)).await;
                }
                ClientMsg::Call(_) => panic!("a watch never calls"),
            }
        }
    }

    async fn mini_node() -> (String, Arc<AtomicUsize>) {
        // Bind before spawning: on the test's current-thread runtime a
        // blocking wait for the spawned task would deadlock.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let conns = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&conns);
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.expect("accept");
                counted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (mut r, mut w) = sock.into_split();
                    let Ok(Some(http::Incoming::Upgrade { key })) =
                        http::read_request(&mut r).await
                    else {
                        return;
                    };
                    http::write_switching_head(
                        &mut w,
                        &codec::accept_key(&key),
                    )
                    .await
                    .expect("101");
                    serve_conn(Messages::new(r), w).await;
                });
            }
        });
        (addr, conns)
    }

    #[tokio::test]
    async fn watches_of_one_identity_share_one_connection() {
        let (addr, conns) = mini_node().await;
        let auth = Auth::Anonymous {
            tenant: U48::from(7u32),
        };

        // Hang guard: any broken path must fail the test, not wedge it.
        macro_rules! within {
            ($fut:expr) => {
                tokio::time::timeout(std::time::Duration::from_secs(30), $fut)
                    .await
                    .expect("timed out")
            };
        }

        // Three watches, two topics, one identity — and later, a trigger.
        let (mut a1, g1) =
            within!(watch(&addr, auth.clone(), WatchMode::WebSocket, TOPIC_A))
                .expect("watch a1");
        let (mut b, g2) =
            within!(watch(&addr, auth.clone(), WatchMode::WebSocket, TOPIC_B))
                .expect("watch b");
        let (mut a2, g3) =
            within!(watch(&addr, auth.clone(), WatchMode::WebSocket, TOPIC_A))
                .expect("watch a2 — shared topic acks locally");
        let (_trigger_rx, g4) =
            within!(watch(&addr, auth.clone(), WatchMode::WebSocket, TRIGGER))
                .expect("trigger");

        assert_eq!(within!(a1.next()), Some(event(TOPIC_A)), "first A watcher");
        assert_eq!(within!(a2.next()), Some(event(TOPIC_A)), "fanned out");
        assert_eq!(within!(b.next()), Some(event(TOPIC_B)));
        assert_eq!(
            conns.load(Ordering::SeqCst),
            1,
            "one identity = one connection, however many watches"
        );

        // Dropping every guard closes the connection; the next watch of
        // the same identity dials a fresh one — never a dying actor.
        drop((g1, g2, g3, g4));
        let (_rx, _g) =
            within!(watch(&addr, auth, WatchMode::WebSocket, TOPIC_A))
                .expect("fresh watch after teardown");
        assert_eq!(conns.load(Ordering::SeqCst), 2, "a fresh dial");
    }
}
