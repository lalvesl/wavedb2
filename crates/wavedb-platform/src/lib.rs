//! `wavedb-platform` — the native ⇄ browser seam.
//!
//! Everything above this crate is target-independent: core mints ids, net
//! frames requests, the client streams replies — none of them may name
//! `SystemTime`, a socket, or `fetch`. This crate owns the per-target
//! facts behind one API compiled two ways:
//!
//! - [`time`] — the wall clock (`SystemTime` / `Date.now()`). On
//!   wasm32-unknown-unknown `SystemTime::now()` **panics at runtime**, so
//!   this seam is correctness, not style;
//! - [`rand`] — entropy (`RandomState` hasher keys / `crypto.getRandomValues`);
//! - [`http`] — the **client half** of the dumb tunnel (hand-rolled HTTP/1.1
//!   POST over `TcpStream` / browser `fetch` + `Request` with a streamed
//!   response body);
//! - [`ws`] — the **client half** of the WebSocket transport (hand-rolled
//!   RFC 6455 over `TcpStream` / the browser's own `WebSocket` object).
//!
//! Same module paths, same signatures, two implementations — conditional
//! compilation is the dispatch (no traits, no `dyn`). The server halves of
//! both transports stay in `wavedb-net`: a node is never a browser.

// Browser futures hold `JsValue`s, which are never `Send`; the native client
// path runs on the same current-thread model as the engine. Established
// stance across the workspace.
#![allow(clippy::future_not_send)]

pub mod error;
pub mod http;
pub mod rand;
pub mod time;
pub mod ws;

pub use error::{Error, Result};
