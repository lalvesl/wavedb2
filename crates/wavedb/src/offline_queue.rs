//! The client offline write queue (W8 — [RFC 0036]) behind [`Db::open`].
//!
//! A write that refuses **transport** (the node is unreachable) is appended
//! here instead of surfacing the fault, so the op succeeds *provisionally*
//! (mirrored into the local cache) and reaches the node later. On the next
//! command that finds the node, the queue **replays FIFO, node-first** —
//! older writes flush before the new one runs — so the cache converges to the
//! node's authoritative state without a merge.
//!
//! **Conflicts are honest.** A replayed write that lost a race surfaces the
//! `Expect` guard's [`Error::Conflict`] node-side; the drain drops it (never a
//! silent overwrite) and the live-sync catch-up reconciles the cache. A
//! transport fault mid-drain stops the replay (still offline) and keeps the
//! rest for next time.
//!
//! **Scope (this slice).** The queue is held **in memory**: it survives a
//! network blip within one process — the case the write-through cache loses
//! today — but not a process restart. A durable on-store queue and offline
//! `insert`/`update`/`remove` (whose node-minted ids need a reconciliation
//! model) are later phases; see [RFC 0036].
//!
//! [RFC 0036]: https://docs.rs/wavedb
//! [`Db::open`]: crate::Db::open
//! [`Error::Conflict`]: wavedb_core::Error::Conflict

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use wavedb_core::U48;
use wavedb_core::expose::{Command, Reply};

use crate::db::Db;
use crate::error::{Error, Result};

/// One write that could not reach the node, held for replay on reconnect.
/// Carries its own `tenant` so a re-scoped ([`Db::as_tenant`]) drain replays
/// each op under the identity that queued it.
#[derive(Debug, Clone)]
pub struct QueuedOp {
    pub tenant: U48,
    pub struct_hash: u64,
    pub command: Command,
    pub payload: Vec<u8>,
}

/// A client's FIFO of offline writes awaiting replay. Shared across a `Db`'s
/// clones (and its [`as_tenant`](Db::as_tenant) re-scopes) so any handle
/// drains what any queued.
#[derive(Debug, Default)]
pub struct OfflineQueue {
    ops: Mutex<VecDeque<QueuedOp>>,
    /// Guards against two concurrent drains double-sending the front op.
    draining: AtomicBool,
}

impl OfflineQueue {
    /// Append a write to the back of the queue.
    pub fn push(&self, op: QueuedOp) {
        if let Ok(mut ops) = self.ops.lock() {
            ops.push_back(op);
        }
    }

    /// How many writes are waiting to replay.
    pub fn len(&self) -> usize {
        self.ops.lock().map_or(0, |ops| ops.len())
    }

    fn front(&self) -> Option<QueuedOp> {
        self.ops.lock().ok().and_then(|ops| ops.front().cloned())
    }

    fn pop_front(&self) {
        if let Ok(mut ops) = self.ops.lock() {
            ops.pop_front();
        }
    }
}

impl Db {
    /// Queue a write that refused transport for node-first replay (W8) — only
    /// meaningful on a cache-backed handle, whose caller already mirrored the
    /// value locally.
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn enqueue_offline(
        &self,
        struct_hash: u64,
        command: Command,
        payload: Vec<u8>,
    ) {
        self.offline_queue().push(QueuedOp {
            tenant: self.tenant(),
            struct_hash,
            command,
            payload,
        });
    }

    /// Replay every write queued while offline (W8), FIFO and node-first,
    /// through this handle's command path; returns how many reached the node.
    /// Called automatically before a `save`, and exposed so an app can force a
    /// sync (e.g. on its own reconnect signal). A transport fault mid-drain
    /// stops the replay and keeps the rest for next time; a node refusal drops
    /// the losing op (the live-sync catch-up reconciles the cache).
    pub async fn drain_offline_queue(&self) -> usize {
        drain(self.offline_queue(), self).await
    }

    /// How many writes are queued offline awaiting replay (W8).
    #[must_use]
    pub fn offline_pending(&self) -> usize {
        self.offline_queue().len()
    }
}

/// `true` = the replay is still offline (a transport fault), so stop the drain
/// and keep the op; `false` = the node answered authoritatively (success, or a
/// refusal like [`Error::Conflict`] we do not retry), so drop it and continue.
const fn still_offline(result: &Result<Reply>) -> bool {
    matches!(result, Err(Error::Transport(_)))
}

/// Replay the queue FIFO through `db`'s command path, node-first: stop at the
/// first transport fault (keep the rest), drop every op the node answered
/// authoritatively. Returns how many left the queue. A concurrent drain is a
/// no-op (the reentrancy guard).
pub async fn drain(queue: &OfflineQueue, db: &Db) -> usize {
    if queue.draining.swap(true, Ordering::AcqRel) {
        return 0; // another drain already owns the front of the queue
    }
    let mut flushed = 0;
    while let Some(op) = queue.front() {
        let sent = db
            .as_tenant(op.tenant)
            .command(op.struct_hash, op.command, op.payload)
            .await;
        if still_offline(&sent) {
            break; // still unreachable — replay the rest next time
        }
        queue.pop_front();
        flushed += 1;
    }
    queue.draining.store(false, Ordering::Release);
    flushed
}

#[cfg(test)]
mod tests {
    use wavedb_core::U48;
    use wavedb_core::expose::{Command, Reply};
    use wavedb_net::frame::{NodeError, NodeErrorKind};

    use super::{OfflineQueue, QueuedOp, still_offline};
    use crate::error::Error;

    fn op(hash: u64) -> QueuedOp {
        QueuedOp {
            tenant: U48::from(1u32),
            struct_hash: hash,
            command: Command::Save,
            payload: Vec::new(),
        }
    }

    #[test]
    fn the_queue_is_fifo() {
        let q = OfflineQueue::default();
        q.push(op(0xA));
        q.push(op(0xB));
        assert_eq!(q.len(), 2);
        assert_eq!(q.front().unwrap().struct_hash, 0xA);
        q.pop_front();
        assert_eq!(q.front().unwrap().struct_hash, 0xB, "oldest first");
    }

    #[test]
    fn only_a_transport_fault_stops_the_drain() {
        // A transport fault means still offline — keep the op, stop replaying.
        assert!(still_offline(&Err(Error::Transport(
            wavedb_net::Error::Http("down")
        ))));
        // Success drops the op and the drain moves on.
        assert!(!still_offline(&Ok(Reply::Done)));
        // A node refusal (e.g. a lost Conflict race) is authoritative — drop
        // it too, never a silent retry/overwrite.
        let conflict = NodeError {
            kind: NodeErrorKind::Conflict,
            struct_hash: 0xA,
            message: "lost the race".into(),
        };
        assert!(!still_offline(&Err(Error::Node(conflict))));
    }
}
