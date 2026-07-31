//! The HTTP-poll sync exchange — how a POST-only client watches (M7),
//! **stateless since W6**.
//!
//! A watch over plain HTTP has no connection to push down, so the client's
//! connection manager asks **"anything new?"** on a timer: one ordinary
//! POST whose [`CommandFrame`] carries the reserved [`SYNC_STRUCT_HASH`]
//! and a [`SyncRequest`] payload. The node holds **no session state**: each
//! declared topic rides with the client's cursor (the greatest node
//! instant it has seen there), and the node answers by *navigating* the
//! disk — the collection recency/dead log tails, the Unique chain forward
//! — via each type's `Changes` step. The [`SyncReply`] carries the events
//! plus every topic's advanced cursor.
//!
//! A topic with `since: None` is a **registration**: the node answers its
//! current tail as the starting cursor and no events — a fresh watch
//! begins at "now". Nothing is buffered, nothing expires, and a node
//! restart loses nothing: the next tick navigates the same disk.
//!
//! Polling still requires an authenticated identity (an anonymous sync
//! refuses uniformly), matching the WebSocket subscribe.
//!
//! The hash is **reserved**: the node routes it before the registry, like
//! functions it lives in the one `STRUCT_HASH` space, and the frame
//! command is a filler.
//!
//! [`CommandFrame`]: crate::frame::CommandFrame

use wavedb_wire::WaveWire;

use crate::ws::{RecordEvent, Topic};

/// The reserved hash the sync exchange rides — never in any registry.
pub const SYNC_STRUCT_HASH: u64 = u64::from_le_bytes(*b"WDB.SYNC");

/// One watched topic with the caller's position in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, WaveWire)]
pub struct TopicCursor {
    /// The watched topic.
    pub topic: Topic,
    /// The greatest node instant the caller has seen for it; `None` =
    /// register — answer the current tail, deliver nothing.
    pub since: Option<u64>,
}

/// The poll body: the caller's complete declared subscriptions, each with
/// its cursor — the whole declaration, re-sent each tick.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub struct SyncRequest {
    /// Every topic the caller currently watches.
    pub topics: Vec<TopicCursor>,
}

/// The poll answer: everything that changed past each cursor, in commit
/// order per topic, plus each topic's advanced cursor.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub struct SyncReply {
    /// The navigated changes, as subscription events.
    pub events: Vec<RecordEvent>,
    /// Each requested topic's cursor after this answer — the client sends
    /// these back on its next tick.
    pub cursors: Vec<(Topic, u64)>,
}

#[cfg(test)]
mod tests {
    use wavedb_core::{Id, LocalId, U48};
    use wavedb_wire::{from_wire, to_wire};

    use super::{SYNC_STRUCT_HASH, SyncReply, SyncRequest, TopicCursor};
    use crate::ws::{EventKind, RecordEvent, Topic};

    #[test]
    fn sync_envelopes_roundtrip_on_the_wire() {
        let topic = Topic {
            struct_hash: 0xFEED,
            pivot: Some(LocalId::new(3, false, 1)),
        };
        let request = SyncRequest {
            topics: vec![
                TopicCursor {
                    topic,
                    since: Some(41),
                },
                TopicCursor { topic, since: None },
            ],
        };
        assert_eq!(
            from_wire::<SyncRequest>(&to_wire(&request)).unwrap(),
            request
        );
        let reply = SyncReply {
            events: vec![RecordEvent {
                topic,
                id: Id::new(9, U48::from(2u32), false, 0),
                kind: EventKind::Removed(77),
                meta: None,
                body: Vec::new(),
            }],
            cursors: vec![(topic, 77)],
        };
        assert_eq!(from_wire::<SyncReply>(&to_wire(&reply)).unwrap(), reply);
    }

    #[test]
    fn the_reserved_hash_is_pinned() {
        // Identity-load-bearing: both sides route on this exact value.
        assert_eq!(SYNC_STRUCT_HASH, u64::from_le_bytes(*b"WDB.SYNC"));
    }
}
