//! The **client half** of the dumb tunnel: POST one body, stream the
//! response's bytes back as the peer produces them.
//!
//! Both targets expose the same two names:
//!
//! - [`post(addr, body)`](post) — send the request, check the `200` head,
//!   and hand back the body;
//! - [`Body::chunk`] — the next run of body bytes in arrival order, `None`
//!   at the peer's end of stream.
//!
//! Chunk boundaries carry **no meaning** — they are whatever the socket or
//! the fetch stream delivered. Framing (`[len u32 LE][bytes]`) is
//! `wavedb-net`'s job on top.
//!
//! Native speaks hand-rolled HTTP/1.1 to `host:port` on a fresh
//! connection; the browser goes through `fetch` + `Request` (a bare
//! `host:port` gets `http://` prepended, a full URL passes through). The
//! server half lives in `wavedb-net::http` — a node is never a browser.

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub use native::{Body, post};

#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub use wasm::{Body, post};
