//! The manager task's routing loop — single-threaded, never ends.
//!
//! Each POST becomes its own subtask (exchanges run concurrently; the
//! manager never serialises requests behind one another); each watched
//! `(addr, identity)` gets one long-lived actor ([`ws_conn`] or [`poll`])
//! the loop routes watch registrations to.
//!
//! **Lifecycle authority lives here**, not in the actors: the loop counts
//! each actor's watchers (by watch id) and drops the actor's channel when
//! the last one unregisters — the actor drains what is already queued and
//! exits, and a *later* watch necessarily arrives after the removal, so it
//! always gets a fresh actor (no ack can race a dying connection). An
//! actor dead for its own reasons (the socket broke) is detected by its
//! closed channel and replaced on the next watch — reconnection is a fresh
//! dial.

use std::collections::{HashMap, HashSet};

use futures::StreamExt;
use futures::channel::{mpsc, oneshot};

use super::{Cmd, ConnKey, PostFrames, WatchMode, poll, ws_conn};
use crate::Result;
use crate::frames::FrameReader;

/// One live actor: its channel plus the watch ids riding it. The set (not
/// a bare count) makes stale unregistrations — a guard of a connection
/// that already died — harmless to a respawned successor.
struct Entry<C> {
    tx: mpsc::UnboundedSender<C>,
    watchers: HashSet<u64>,
}

/// Run the manager until its command channel closes (it never does — the
/// process-wide handle holds the sender).
pub(super) async fn run(mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut ws_conns: HashMap<ConnKey, Entry<ws_conn::ConnCmd>> =
        HashMap::new();
    let mut polls: HashMap<ConnKey, Entry<poll::PollCmd>> = HashMap::new();
    while let Some(cmd) = rx.next().await {
        match cmd {
            Cmd::Post {
                addr,
                body,
                connected,
            } => {
                wavedb_platform::task::spawn_local(run_post(
                    addr, body, connected,
                ));
            }
            Cmd::Watch {
                key,
                mode,
                topic,
                watch,
                events,
                ack,
            } => match mode {
                WatchMode::WebSocket => register(
                    &mut ws_conns,
                    key,
                    watch,
                    ws_conn::ConnCmd::Subscribe {
                        topic,
                        watch,
                        events,
                        ack,
                    },
                    ws_conn::spawn,
                ),
                WatchMode::HttpPoll(every) => register(
                    &mut polls,
                    key,
                    watch,
                    poll::PollCmd::Subscribe {
                        topic,
                        watch,
                        events,
                        ack,
                    },
                    |key| poll::spawn(key, every),
                ),
            },
            Cmd::Unwatch {
                key,
                poll: is_poll,
                topic,
                watch,
            } => {
                if is_poll {
                    unregister(
                        &mut polls,
                        &key,
                        watch,
                        poll::PollCmd::Unsubscribe { topic, watch },
                    );
                } else {
                    unregister(
                        &mut ws_conns,
                        &key,
                        watch,
                        ws_conn::ConnCmd::Unsubscribe { topic, watch },
                    );
                }
            }
        }
    }
}

/// Deliver a watch registration to `key`'s actor, spawning one (or
/// replacing a dead one — its channel closed when its connection broke).
fn register<C>(
    actors: &mut HashMap<ConnKey, Entry<C>>,
    key: ConnKey,
    watch: u64,
    cmd: C,
    spawn: impl FnOnce(&ConnKey) -> mpsc::UnboundedSender<C>,
) {
    let mut pending = cmd;
    if let Some(entry) = actors.get_mut(&key) {
        match entry.tx.unbounded_send(pending) {
            Ok(()) => {
                entry.watchers.insert(watch);
                return;
            }
            Err(returned) => pending = returned.into_inner(),
        }
    }
    let tx = spawn(&key);
    let _ = tx.unbounded_send(pending);
    let watchers = match actors.remove(&key) {
        // Keep the dead predecessor's ids: their guards' unregistrations
        // are still coming and must not count against the successor.
        Some(dead) => dead.watchers,
        None => HashSet::new(),
    };
    let mut entry = Entry { tx, watchers };
    entry.watchers.insert(watch);
    actors.insert(key, entry);
}

/// Unregister a watcher; when the last one leaves, drop the actor's
/// channel — it drains what is queued and exits, closing its connection.
fn unregister<C>(
    actors: &mut HashMap<ConnKey, Entry<C>>,
    key: &ConnKey,
    watch: u64,
    cmd: C,
) {
    let Some(entry) = actors.get_mut(key) else {
        return; // Already gone — nothing was watching.
    };
    let _ = entry.tx.unbounded_send(cmd);
    entry.watchers.remove(&watch);
    if entry.watchers.is_empty() {
        actors.remove(key);
    }
}

/// One POSTed exchange: dial + send, answer `connected` with the outcome,
/// then pump the response frames into the caller's channel.
async fn run_post(
    addr: String,
    body: Vec<u8>,
    connected: oneshot::Sender<Result<PostFrames>>,
) {
    let mut frames = match wavedb_platform::http::post(&addr, &body).await {
        Ok(stream) => FrameReader::new(stream),
        Err(e) => {
            let _ = connected.send(Err(e.into()));
            return;
        }
    };
    let (tx, rx) = mpsc::unbounded();
    if connected.send(Ok(PostFrames { rx })).is_err() {
        return; // The caller went away before the head arrived.
    }
    loop {
        match frames.next_frame().await {
            Ok(Some(frame)) => {
                if tx.unbounded_send(Ok(frame)).is_err() {
                    return; // The caller stopped reading mid-stream.
                }
            }
            // A clean end closes the channel by dropping the sender.
            Ok(None) => return,
            Err(e) => {
                let _ = tx.unbounded_send(Err(e));
                return;
            }
        }
    }
}
