//! The HTTP-poll sync exchange — how a POST-only client watches (M7).
//!
//! A watch over plain HTTP has no connection to push down, so the client's
//! connection manager asks **"anything new?"** on a timer: one ordinary
//! POST whose [`CommandFrame`] carries the reserved [`SYNC_STRUCT_HASH`]
//! and a [`SyncRequest`] payload. The node answers from its per-session
//! event buffer with a [`SyncReply`] (riding `Reply::Returned`, like a
//! function call).
//!
//! Every poll declares the **whole** topic list and the node **replaces**
//! the session's set with it — registration is stateless-idempotent, so a
//! node restart heals on the next tick and a dropped topic stops buffering
//! on the next tick, with nothing incremental to track on either side. The
//! buffer is keyed by the access token's `session` claim: polling requires
//! an authenticated identity (an anonymous sync refuses uniformly), and
//! two clients must present distinct session ids to poll independently —
//! real logins always do.
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

/// The poll body: the caller's complete declared subscriptions. The node
/// replaces the session's set with this list, then drains its buffer.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub struct SyncRequest {
    /// Every topic the caller currently watches — the whole declaration,
    /// re-sent each tick.
    pub subscribe: Vec<Topic>,
}

/// The poll answer: the events buffered for this session since its last
/// drain, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub struct SyncReply {
    /// Buffered subscription events, in commit order.
    pub events: Vec<RecordEvent>,
}

#[cfg(test)]
mod tests {
    use wavedb_core::{Id, LocalId, U48};
    use wavedb_wire::{from_wire, to_wire};

    use super::{SYNC_STRUCT_HASH, SyncReply, SyncRequest};
    use crate::ws::{EventKind, RecordEvent, Topic};

    #[test]
    fn sync_envelopes_roundtrip_on_the_wire() {
        let topic = Topic {
            struct_hash: 0xFEED,
            pivot: Some(LocalId::new(3, false, 1)),
        };
        let request = SyncRequest {
            subscribe: vec![topic],
        };
        assert_eq!(
            from_wire::<SyncRequest>(&to_wire(&request)).unwrap(),
            request
        );
        let reply = SyncReply {
            events: vec![RecordEvent {
                topic,
                id: Id::new(9, U48::from(2u32), false, 0),
                kind: EventKind::Saved,
                body: vec![1, 2],
            }],
        };
        assert_eq!(from_wire::<SyncReply>(&to_wire(&reply)).unwrap(), reply);
    }

    #[test]
    fn the_reserved_hash_is_pinned() {
        // Identity-load-bearing: both sides route on this exact value.
        assert_eq!(SYNC_STRUCT_HASH, u64::from_le_bytes(*b"WDB.SYNC"));
    }
}
