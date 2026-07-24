//! The platform layer's typed error.

use thiserror::Error;

/// Faults from the platform seams: socket or browser I/O, and a response
/// head that was not the minimal HTTP the tunnel speaks.
///
/// Anything *inside* a successfully transported body (frames, envelopes,
/// refusals) belongs to `wavedb-net` — the layers convert at that seam
/// (`platform::Error` → `net::Error::Platform`).
#[derive(Debug, Error)]
pub enum Error {
    /// The peer's bytes were not the minimal HTTP/1.1 the tunnel speaks.
    #[error("malformed http: {0}")]
    Http(&'static str),
    /// A non-200 response head — transport-level rejection (a WaveDB
    /// refusal would be a 200 carrying a `NodeError` envelope).
    #[error("http status {0}")]
    Status(u16),
    /// A socket-level fault (connect, read, write).
    #[cfg(not(target_arch = "wasm32"))]
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// A browser API refusal (`fetch`, streams, `crypto`), stringified —
    /// `JsValue` carries no type to keep.
    #[cfg(target_arch = "wasm32")]
    #[error("browser api: {0}")]
    Js(String),
}

/// Shorthand for a `Result` carrying the platform [`Error`](enum@Error).
pub type Result<T> = core::result::Result<T, Error>;

/// Wrap a thrown `JsValue` with the API call that threw it.
#[cfg(target_arch = "wasm32")]
pub(crate) fn js(
    context: &'static str,
    value: &wasm_bindgen::JsValue,
) -> Error {
    Error::Js(format!("{context}: {value:?}"))
}
