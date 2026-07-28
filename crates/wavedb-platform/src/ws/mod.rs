//! The **client half** of the WebSocket transport: open a connection,
//! exchange binary messages.
//!
//! Both targets expose the same two names:
//!
//! - [`connect(addr)`](connect) — dial, run the RFC 6455 handshake, hand
//!   back the open connection;
//! - [`Conn`] — `send(bytes)` / `recv() -> Option<bytes>` / `close()`.
//!
//! Messages are **binary only** and carry `wavedb-net`'s WebSocket
//! envelopes; identity (the `Hello` token presentation) lives there too —
//! this layer only moves bytes, the same dumb-tunnel stance as
//! [`http`](crate::http).
//!
//! Native is a hand-rolled RFC 6455 exchange over a fresh `TcpStream`
//! (the [`codec`] module, shared with the server half in
//! `wavedb-net::ws`); the browser rides its own `WebSocket` object, which
//! frames, masks, and answers pings itself. The server half stays in
//! `wavedb-net::ws` — a node is never a browser.

#[cfg(not(target_arch = "wasm32"))]
pub mod codec;
#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{Conn, RecvHalf, SendHalf, connect};

#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub use wasm::{Conn, RecvHalf, SendHalf, connect};

/// One message off the receiving half of a split connection
/// ([`Conn::split`]): a binary payload, or a ping the sending half must
/// answer ([`SendHalf::pong`] — surfaced on native only; the browser pongs
/// itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Received {
    /// A binary message — a `wavedb-net` envelope.
    Binary(Vec<u8>),
    /// A ping to answer.
    Ping(Vec<u8>),
}
