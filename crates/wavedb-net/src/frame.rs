//! The self-contained request/response envelopes.
//!
//! **One uniform frame** carries every operation: a record command today, a
//! `#[server]` function call in M4 — functions and structs share the
//! `STRUCT_HASH` space, so nothing at the frame level tells them apart. The
//! transport is a dumb tunnel: identity, the command, and errors all ride
//! *inside* these wire values, never in HTTP headers or status codes.

use wavedb_core::expose::{Command, Reply};
use wavedb_core::{Error as CoreError, U48};
use wavedb_wire::WaveWire;

/// One operation to run on the node.
///
/// Which item (`struct_hash`), which op (`command` — ignored by a
/// server-function arm, whose hash *is* the operation), and the operation's
/// payload bytes (a record body, an `Id`, or an args tuple).
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub struct CommandFrame {
    /// A `#[wavedb]` struct or (M4) a `#[server]` fn — one hash space.
    pub struct_hash: u64,
    /// The struct op; a function arm ignores it.
    pub command: Command,
    /// The op's wire payload, decoded by the matched arm.
    pub payload: Vec<u8>,
}

/// A complete request — everything the node needs, in the POST body.
///
/// `tenant` is a **placeholder identity** until M8: the node trusts the
/// claimed tenant as the session binding. M8 replaces it with a verified
/// HMAC access token carried in this same envelope (pre-release wire
/// layouts change freely — no versioning).
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub struct Request {
    /// The claimed tenant (M8: derived from the verified access token).
    pub tenant: U48,
    /// The operation.
    pub frame: CommandFrame,
}

/// What kind of node-side failure a [`NodeError`] reports — the typed half
/// of the core error, flattened for the wire (evidence rides in `message`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, WaveWire)]
pub enum NodeErrorKind {
    /// Unlisted type, excluded op, or wrong-shape command — uniform refusal.
    UnknownStructHash,
    /// The payload did not decode as the declared type.
    Wire,
    /// A collection handle's `Pivot` record was missing.
    PivotMissing,
    /// An index pointed at a record the store no longer holds.
    RecordMissing,
    /// A dangling `BpTree` node pointer.
    BpTreeNodeMissing,
    /// A value read as a `BpTree` node had a foreign tag.
    BpTreeNodeBadTag,
    /// A secondary-index lookup out of the declared range.
    SecondaryIndexOutOfRange,
    /// A value did not fit in 48 bits.
    U48Overflow,
    /// A storage-backend fault (disk I/O, corruption).
    Backend,
}

/// A structured node-side rejection, riding **inside** the 200 response —
/// HTTP status stays transport-only.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire, thiserror::Error)]
#[error("node error ({kind:?}) on {struct_hash:#018x}: {message}")]
pub struct NodeError {
    /// The failure class.
    pub kind: NodeErrorKind,
    /// The frame's `struct_hash` the failure is about.
    pub struct_hash: u64,
    /// Human-readable evidence (the core error's display).
    pub message: String,
}

impl NodeError {
    /// Flatten a core error into its wire shape, stamped with the frame's
    /// `struct_hash`.
    #[must_use]
    pub fn from_core(struct_hash: u64, err: &CoreError) -> Self {
        let kind = match err {
            CoreError::Wire(_) => NodeErrorKind::Wire,
            CoreError::U48Overflow(_) => NodeErrorKind::U48Overflow,
            CoreError::UnknownStructHash(_) => NodeErrorKind::UnknownStructHash,
            CoreError::BpTreeNodeMissing(_) => NodeErrorKind::BpTreeNodeMissing,
            CoreError::BpTreeNodeBadTag(_) => NodeErrorKind::BpTreeNodeBadTag,
            CoreError::PivotMissing(_) => NodeErrorKind::PivotMissing,
            CoreError::RecordMissing(_) => NodeErrorKind::RecordMissing,
            CoreError::SecondaryIndexOutOfRange(_) => {
                NodeErrorKind::SecondaryIndexOutOfRange
            }
            CoreError::Backend(_) => NodeErrorKind::Backend,
        };
        Self {
            kind,
            struct_hash,
            message: err.to_string(),
        }
    }
}

/// The node's answer: the executed command's [`Reply`], or a structured
/// refusal. Always shipped in a 200 — an HTTP-level failure means the
/// *transport* broke, not the operation.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub enum Response {
    /// The command executed; its reply.
    Ok(Reply),
    /// The node refused or failed the command.
    Err(NodeError),
}

impl Response {
    /// Convert into a plain `Result`.
    ///
    /// # Errors
    /// The [`NodeError`] the node shipped, when the response is `Err`.
    pub fn into_result(self) -> core::result::Result<Reply, NodeError> {
        match self {
            Self::Ok(reply) => Ok(reply),
            Self::Err(e) => Err(e),
        }
    }
}

/// One frame of a response body.
///
/// Every response is a **sequence** of these (`[len u32 LE][wire]` each):
/// zero or more [`Item`](Self::Item)s — a walk's records, written as the
/// node produces them — then exactly one [`End`](Self::End) carrying the
/// exchange's final word. A scalar command is the degenerate case: no items,
/// just the `End`. A fault mid-walk ends the stream early with
/// `End(Err(..))` after the items already shipped.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub enum StreamFrame {
    /// One walk item's wire bytes (a record body, or a `(Metadata, body)`
    /// pair for a history walk).
    Item(Vec<u8>),
    /// The final word; nothing follows on the connection.
    End(Response),
}

#[cfg(test)]
mod tests {
    use wavedb_core::expose::{Command, Reply};
    use wavedb_core::{Error as CoreError, U48};
    use wavedb_wire::{from_wire, to_wire};

    use super::{CommandFrame, NodeError, NodeErrorKind, Request, Response};

    fn request() -> Request {
        Request {
            tenant: U48::from(42u32),
            frame: CommandFrame {
                struct_hash: 0xABCD,
                command: Command::Save,
                payload: vec![1, 2, 3],
            },
        }
    }

    #[test]
    fn request_roundtrips_on_the_wire() {
        let r = request();
        assert_eq!(from_wire::<Request>(&to_wire(&r)).unwrap(), r);
    }

    #[test]
    fn response_roundtrips_both_arms() {
        let ok = Response::Ok(Reply::Value(Some(vec![9])));
        assert_eq!(from_wire::<Response>(&to_wire(&ok)).unwrap(), ok);

        let err = Response::Err(NodeError {
            kind: NodeErrorKind::UnknownStructHash,
            struct_hash: 0xFEED,
            message: "unknown struct hash".into(),
        });
        assert_eq!(from_wire::<Response>(&to_wire(&err)).unwrap(), err);
    }

    #[test]
    fn truncated_request_is_a_wire_error() {
        let bytes = to_wire(&request());
        assert!(from_wire::<Request>(&bytes[..bytes.len() - 2]).is_err());
    }

    #[test]
    fn node_error_flattens_core_variants() {
        let e = NodeError::from_core(7, &CoreError::UnknownStructHash(0xBEEF));
        assert_eq!(e.kind, NodeErrorKind::UnknownStructHash);
        assert_eq!(e.struct_hash, 7);
        assert!(e.message.contains("unknown struct hash"));

        let e = NodeError::from_core(7, &CoreError::Backend("disk".into()));
        assert_eq!(e.kind, NodeErrorKind::Backend);
        assert!(e.message.contains("disk"));
    }

    #[test]
    fn response_into_result_maps_arms() {
        assert_eq!(
            Response::Ok(Reply::Done).into_result().unwrap(),
            Reply::Done
        );
        let err = NodeError {
            kind: NodeErrorKind::Backend,
            struct_hash: 1,
            message: "x".into(),
        };
        assert_eq!(Response::Err(err.clone()).into_result().unwrap_err(), err);
    }
}
