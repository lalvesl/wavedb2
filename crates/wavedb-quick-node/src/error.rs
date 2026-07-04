//! The node's setup/serve error type.
//!
//! This covers **standing up and running** a node (opening the engine,
//! binding the socket, the accept loop). Per-request failures never reach
//! here — a refused hash or an engine fault rides back as the
//! [`Response::Err`](wavedb_net::Response) arm of a served response.

use thiserror::Error;
use wavedb_storage::StorageError;

/// A failure while opening or serving a node.
#[derive(Debug, Error)]
pub enum ServerError {
    /// Opening the page engine failed (busy, corruption, filesystem).
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// A transport-layer fault from the accept loop.
    #[error(transparent)]
    Net(#[from] wavedb_net::Error),
    /// Binding the listener socket failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Shorthand for a `Result` carrying a [`ServerError`].
pub type Result<T> = core::result::Result<T, ServerError>;
