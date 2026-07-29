//! One WebSocket connection actor — the identity presented once, N topics
//! multiplexed over it, events fanned out to every watcher of their topic.
//!
//! The socket splits: a reader subtask owns the receive half (frame reads
//! are not cancel-safe — the same reason the node's session loop has one)
//! and forwards messages over a channel; the actor selects over that and
//! its command channel, owning the send half. Subscribes are acked by the
//! node's `TopicOk` (FIFO ⇒ once acked, no later mutation can be missed);
//! a topic is wire-subscribed once, no matter how many watchers share it,
//! and wire-unsubscribed when its last watcher leaves. The actor ends when
//! the socket does or when its last topic empties — dropping the watcher
//! channels (streams end) and any pending acks; the manager replaces it on
//! the next watch.

use std::collections::HashMap;

use futures::channel::{mpsc, oneshot};
use futures::{StreamExt, select};
use wavedb_platform::ws::{self, Received, SendHalf};
use wavedb_wire::{from_wire, to_wire};

use super::ConnKey;
use crate::error::{Error, Result};
use crate::ws::{ClientMsg, RecordEvent, ServerMsg, Topic};

/// What the manager routes to a connection actor.
pub(super) enum ConnCmd {
    Subscribe {
        topic: Topic,
        watch: u64,
        events: mpsc::UnboundedSender<RecordEvent>,
        ack: oneshot::Sender<Result<()>>,
    },
    Unsubscribe {
        topic: Topic,
        watch: u64,
    },
}

/// One topic's client-side state on this connection.
#[derive(Default)]
struct TopicState {
    /// The node acked the subscription — events can arrive.
    live: bool,
    /// The watchers fanned out to, keyed by watch id.
    subs: Vec<(u64, mpsc::UnboundedSender<RecordEvent>)>,
    /// Subscribe acks awaiting the node's `TopicOk`.
    pending: Vec<oneshot::Sender<Result<()>>>,
}

/// Spawn the actor for `key`; the returned sender is the manager's route
/// to it (its closing is how a dead actor is detected).
pub(super) fn spawn(key: &ConnKey) -> mpsc::UnboundedSender<ConnCmd> {
    let (tx, rx) = mpsc::unbounded();
    let key = key.clone();
    wavedb_platform::task::spawn_local(run(key, rx));
    tx
}

async fn run(key: ConnKey, mut cmds: mpsc::UnboundedReceiver<ConnCmd>) {
    let (mut send, mut msgs) = match open(&key).await {
        Ok(halves) => halves,
        Err(error) => {
            fail_queued(&mut cmds, error);
            return;
        }
    };
    let mut topics: HashMap<Topic, TopicState> = HashMap::new();
    loop {
        select! {
            cmd = cmds.next() => {
                // Channel end = the manager dropped this actor (its last
                // watcher unregistered, everything queued is drained).
                let Some(cmd) = cmd else { break };
                if handle_cmd(&mut send, &mut topics, cmd).await.is_err() {
                    break;
                }
            }
            msg = msgs.next() => {
                let Some(msg) = msg else { break }; // Socket ended.
                if handle_msg(&mut send, &mut topics, msg).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = send.close().await;
    // Dropping `topics` drops every event sender (watch streams end) and
    // every pending ack (their watch() calls error out).
}

/// Dial, upgrade, present the identity, verify `HelloOk`, and hand the
/// receive half to a reader subtask.
async fn open(
    key: &ConnKey,
) -> Result<(SendHalf, mpsc::UnboundedReceiver<Received>)> {
    let conn = ws::connect(&key.addr).await?;
    let (mut recv, mut send) = conn.split();
    send.send(&to_wire(&ClientMsg::Hello(key.auth.clone())))
        .await?;
    loop {
        match recv.next().await? {
            Some(Received::Binary(bytes))
                if from_wire::<ServerMsg>(&bytes) == Ok(ServerMsg::HelloOk) =>
            {
                break;
            }
            Some(Received::Ping(payload)) => send.pong(&payload).await?,
            // The node refuses a bad identity by closing without a word.
            _ => return Err(Error::Http("websocket hello refused")),
        }
    }
    let (msg_tx, msg_rx) = mpsc::unbounded();
    wavedb_platform::task::spawn_local(async move {
        loop {
            match recv.next().await {
                Ok(Some(received)) => {
                    if msg_tx.unbounded_send(received).is_err() {
                        return; // The actor ended first.
                    }
                }
                // Close, clean end, or a fault: dropping `msg_tx` tells
                // the actor the connection is over.
                _ => return,
            }
        }
    });
    Ok((send, msg_rx))
}

/// Fail the commands already queued behind a dial/handshake fault — the
/// first ack gets the real error. New watches respawn a fresh actor.
fn fail_queued(cmds: &mut mpsc::UnboundedReceiver<ConnCmd>, error: Error) {
    cmds.close();
    let mut error = Some(error);
    while let Ok(cmd) = cmds.try_recv() {
        if let ConnCmd::Subscribe { ack, .. } = cmd {
            let _ = ack.send(Err(error
                .take()
                .unwrap_or(Error::Http("websocket connect failed"))));
        }
    }
}

async fn handle_cmd(
    send: &mut SendHalf,
    topics: &mut HashMap<Topic, TopicState>,
    cmd: ConnCmd,
) -> Result<()> {
    match cmd {
        ConnCmd::Subscribe {
            topic,
            watch,
            events,
            ack,
        } => {
            let state = topics.entry(topic).or_default();
            state.subs.push((watch, events));
            if state.live {
                // Already acked by the node — live from this moment.
                let _ = ack.send(Ok(()));
            } else {
                // The wire Subscribe goes out once, with the first watcher.
                let first = state.pending.is_empty();
                state.pending.push(ack);
                if first {
                    send.send(&to_wire(&ClientMsg::Subscribe(topic))).await?;
                }
            }
        }
        ConnCmd::Unsubscribe { topic, watch } => {
            let Some(state) = topics.get_mut(&topic) else {
                return Ok(());
            };
            state.subs.retain(|(id, _)| *id != watch);
            if state.subs.is_empty() && state.pending.is_empty() {
                topics.remove(&topic);
                // The node acks with a TopicOk nobody awaits — ignored.
                send.send(&to_wire(&ClientMsg::Unsubscribe(topic))).await?;
            }
        }
    }
    Ok(())
}

async fn handle_msg(
    send: &mut SendHalf,
    topics: &mut HashMap<Topic, TopicState>,
    msg: Received,
) -> Result<()> {
    let bytes = match msg {
        Received::Ping(payload) => {
            send.pong(&payload).await?;
            return Ok(());
        }
        Received::Binary(bytes) => bytes,
    };
    match from_wire::<ServerMsg>(&bytes) {
        Ok(ServerMsg::TopicOk(topic)) => {
            // An unsubscribe ack finds no entry and is ignored.
            if let Some(state) = topics.get_mut(&topic) {
                state.live = true;
                for ack in state.pending.drain(..) {
                    let _ = ack.send(Ok(()));
                }
            }
            Ok(())
        }
        Ok(ServerMsg::Event(event)) => {
            if let Some(state) = topics.get_mut(&event.topic) {
                // A watcher that dropped its receiver without an Unwatch
                // (its guard is on the way) is pruned here.
                state
                    .subs
                    .retain(|(_, tx)| tx.unbounded_send(event.clone()).is_ok());
            }
            Ok(())
        }
        // A watch connection has no call in flight to be answered —
        // anything else is protocol confusion; drop the connection.
        _ => Err(Error::Http("unexpected websocket message")),
    }
}
