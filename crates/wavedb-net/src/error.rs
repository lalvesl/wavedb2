//! The transport-layer error type.

use thiserror::Error;

/// Errors raised by `wavedb-net` — connection faults, malformed HTTP
/// framing, and wire decode failures on an envelope.
///
/// Faults *inside* a successfully transported envelope (a refused hash, a
/// storage failure) are **not** errors here: they travel as the
/// [`NodeError`](crate::frame::NodeError) arm of a
/// [`Response`](crate::frame::Response) — the transport did its job.
#[derive(Debug, Error)]
pub enum Error {
    /// A socket-level fault (connect, read, write).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The peer's bytes were not the minimal HTTP/1.1 the tunnel speaks.
    #[error("malformed http: {0}")]
    Http(&'static str),
    /// A declared body length exceeded the tunnel's cap.
    #[error("body of {have} bytes exceeds the {limit}-byte cap")]
    BodyTooLarge {
        /// The maximum the tunnel accepts.
        limit: usize,
        /// The length the peer declared.
        have: usize,
    },
    /// The peer answered with a non-200 status — transport-level rejection
    /// (a WaveDB refusal would be a 200 carrying a `NodeError` envelope).
    #[error("http status {0}")]
    Status(u16),
    /// An envelope failed to decode as its wire type.
    #[error(transparent)]
    Wire(#[from] wavedb_wire::Error),
    /// A node-side refusal, surfaced by [`call_ok`](crate::NetClient::call_ok)
    /// (which flattens the `Response::Err` arm into this error type). The
    /// lower-level [`call`](crate::NetClient::call) returns it as a value.
    #[error(transparent)]
    Node(#[from] crate::frame::NodeError),
}

/// Shorthand for a `Result` carrying the transport [`Error`].
pub type Result<T> = core::result::Result<T, Error>;
