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
pub use native::{Conn, connect};

#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub use wasm::{Conn, connect};
