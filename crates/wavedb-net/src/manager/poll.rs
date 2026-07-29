//! The HTTP-poll actor — a watch for clients that cannot hold a WebSocket
//! open: one loop per `(addr, identity)` asking the node "anything new?"
//! on an adjustable timer ([`WatchMode::HttpPoll`](super::WatchMode)).
//!
//! Every tick (and immediately on a new watcher — the first successful
//! sync is the liveness ack) it POSTs a [`SyncRequest`] declaring the
//! **whole** current topic list; the node replaces the session's set and
//! drains its buffer, so registration self-heals across node restarts and
//! dropped topics stop buffering — nothing incremental on either side.
//! Buffered events fan out to watchers exactly like the WebSocket path.
//!
//! Outage semantics differ from a pushed watch on purpose: a **transport**
//! fault is tolerated (polling is loosely coupled — the next tick retries,
//! watchers just see silence), while a **node refusal** (an expired token,
//! an anonymous sync) is authoritative and ends the actor, closing every
//! watcher's stream. Events committed while the node was down or between
//! its restart and the next re-registering tick are missed — the honest
//! pre-W6 gap; the journal-cursor catch-up closes it.

use core::time::Duration;
use std::collections::HashMap;

use futures::channel::{mpsc, oneshot};
use futures::{FutureExt, StreamExt, select};
use wavedb_core::expose::{Command, Reply};
use wavedb_wire::{from_wire, to_wire};

use super::ConnKey;
use crate::error::{Error, Result};
use crate::frame::{CommandFrame, Request, Response, StreamFrame};
use crate::frames::FrameReader;
use crate::sync::{SYNC_STRUCT_HASH, SyncReply, SyncRequest};
use crate::ws::{RecordEvent, Topic};

/// What the manager routes to a poll actor.
pub(super) enum PollCmd {
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

/// Spawn the poll actor for `key`, ticking every `every`.
pub(super) fn spawn(
    key: &ConnKey,
    every: Duration,
) -> mpsc::UnboundedSender<PollCmd> {
    let (tx, rx) = mpsc::unbounded();
    let key = key.clone();
    wavedb_platform::task::spawn_local(run(key, every, rx));
    tx
}

/// The watchers per topic plus the acks a next successful sync completes.
#[derive(Default)]
struct PollState {
    topics: HashMap<Topic, Vec<(u64, mpsc::UnboundedSender<RecordEvent>)>>,
    pending: Vec<oneshot::Sender<Result<()>>>,
}

async fn run(
    key: ConnKey,
    every: Duration,
    mut cmds: mpsc::UnboundedReceiver<PollCmd>,
) {
    let mut state = PollState::default();
    loop {
        let due = select! {
            cmd = cmds.next() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    PollCmd::Subscribe { topic, watch, events, ack } => {
                        state
                            .topics
                            .entry(topic)
                            .or_default()
                            .push((watch, events));
                        state.pending.push(ack);
                        true // Register + drain now — the ack is liveness.
                    }
                    PollCmd::Unsubscribe { topic, watch } => {
                        // The manager drops this actor's channel when the
                        // last watcher leaves (the node's session buffer
                        // then ages out on its own — the poll TTL).
                        if let Some(subs) = state.topics.get_mut(&topic) {
                            subs.retain(|(id, _)| *id != watch);
                            if subs.is_empty() {
                                state.topics.remove(&topic);
                            }
                        }
                        false
                    }
                }
            }
            () = wavedb_platform::time::sleep(every).fuse() => {
                !state.topics.is_empty()
            }
        };
        if due && sync(&key, &mut state).await.is_err() {
            break; // An authoritative refusal — end every watch stream.
        }
    }
}

/// One "anything new?" exchange. `Err` = node refusal (fatal); a transport
/// fault resolves `Ok` after failing only the pending (initial) acks —
/// established watchers ride the outage and the next tick retries.
async fn sync(
    key: &ConnKey,
    state: &mut PollState,
) -> core::result::Result<(), ()> {
    let request = Request {
        auth: key.auth.clone(),
        frame: CommandFrame {
            struct_hash: SYNC_STRUCT_HASH,
            // Like a function call, sync ignores the frame command.
            command: Command::Get,
            payload: to_wire(&SyncRequest {
                subscribe: state.topics.keys().copied().collect(),
            }),
        },
    };
    match exchange(&key.addr, &to_wire(&request)).await {
        Ok(reply) => {
            for event in reply.events {
                if let Some(subs) = state.topics.get_mut(&event.topic) {
                    subs.retain(|(_, tx)| {
                        tx.unbounded_send(event.clone()).is_ok()
                    });
                }
            }
            for ack in state.pending.drain(..) {
                let _ = ack.send(Ok(()));
            }
            Ok(())
        }
        Err(error) => {
            let fatal = matches!(error, Error::Node(_));
            let mut error = Some(error);
            for ack in state.pending.drain(..) {
                let _ = ack.send(Err(error
                    .take()
                    .unwrap_or(Error::Http("sync poll failed"))));
            }
            if fatal { Err(()) } else { Ok(()) }
        }
    }
}

/// POST the sync request and decode its scalar answer.
async fn exchange(addr: &str, body: &[u8]) -> Result<SyncReply> {
    let stream = wavedb_platform::http::post(addr, body).await?;
    let mut frames = FrameReader::new(stream);
    let bytes = frames
        .next_frame()
        .await?
        .ok_or(Error::Http("response ended before its End frame"))?;
    match from_wire::<StreamFrame>(&bytes)? {
        StreamFrame::End(Response::Ok(Reply::Returned(reply))) => {
            Ok(from_wire::<SyncReply>(&reply)?)
        }
        StreamFrame::End(Response::Ok(_)) => {
            Err(Error::Http("sync answered with a non-return reply"))
        }
        StreamFrame::End(Response::Err(refusal)) => Err(Error::Node(refusal)),
        StreamFrame::Item(_) => {
            Err(Error::Http("item frame on a scalar command"))
        }
    }
}
