//! `wavedb-net` — the transport layer.
//!
//! **WaveDB is the wire protocol**: there is no REST/RPC split, no DTO layer.
//! A client serialises a [`Request`] (a tenant identity + one uniform
//! [`CommandFrame`]) and the node deserialises it straight into the engine;
//! the answer comes back as a [`Response`]. Record ops and (M4)
//! `#[server]`-function calls share the *same* frame — functions and structs
//! live in one `STRUCT_HASH` space, so nothing at the frame level tells them
//! apart.
//!
//! The transport is a **dumb tunnel**: identity, the command, and any
//! refusal all ride *inside* the wire envelopes — never in HTTP headers,
//! cookies, or status codes. Two transports are wired: HTTP POST (one
//! exchange per connection, the token re-sent each time) and WebSocket
//! (M7 — the token presented once in [`ws::ClientMsg::Hello`], the
//! connection bound to that identity, subscription
//! [`Event`](ws::ServerMsg::Event)s pushed as mutations land).
//!
//! ## Layers
//!
//! - [`frame`] — the [`Request`] / [`Response`] / [`NodeError`] wire values.
//! - [`frames`] — the response's `[len u32 LE][bytes]` frame sequence, read
//!   over the platform body stream (both targets).
//! - [`ws`] — the WebSocket envelopes (`Hello`/`Call`/`Subscribe` →
//!   `Item`/`End`/`Event`), both targets; the server session loop lives in
//!   `wavedb-quick-node`.
//! - [`http`] — the server half's minimal HTTP/1.1 framing (native only;
//!   the client half lives in `wavedb-platform::http`), now also routing
//!   the WebSocket upgrade.
//! - [`client`] — [`NetClient`], the client half (build → POST → decode);
//!   compiles native (TcpStream) and wasm32 (`fetch`) alike.
//!
//! The **server** half (accepting connections, decoding a `Request`, running
//! the gates + `Exposure::execute`, encoding the `Response`) lives in
//! `wavedb-quick-node`, which owns the storage engine the node dispatches to.

// Browser-side transport futures hold `JsValue`s (never `Send`); the native
// path runs current-thread. Established stance across the workspace.
#![allow(clippy::future_not_send)]

pub mod auth;
pub mod client;
pub mod error;
pub mod frame;
pub mod frames;
pub mod ws;

pub use client::{Executed, NetClient};
pub use error::{Error, Result};
pub use frame::{
    Auth, CommandFrame, NodeError, NodeErrorKind, Request, Response,
};

#[cfg(not(target_arch = "wasm32"))]
pub mod http;
