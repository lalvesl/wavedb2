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
//! cookies, or status codes. The only wired transport is HTTP POST (one
//! exchange per connection); WebSocket, push, and Bloom screen-sync are
//! deferred (M7).
//!
//! ## Layers
//!
//! - [`frame`] — the [`Request`] / [`Response`] / [`NodeError`] wire values.
//! - [`http`] — the minimal HTTP/1.1 framing (native only).
//! - [`client`] — [`NetClient`], the client half (build → POST → decode).
//!
//! The **server** half (accepting connections, decoding a `Request`, running
//! the gates + `Exposure::execute`, encoding the `Response`) lives in
//! `wavedb-quick-node`, which owns the storage engine the node dispatches to.

pub mod auth;
pub mod error;
pub mod frame;

pub use error::{Error, Result};
pub use frame::{
    Auth, CommandFrame, NodeError, NodeErrorKind, Request, Response,
};

#[cfg(not(target_arch = "wasm32"))]
pub mod client;
#[cfg(not(target_arch = "wasm32"))]
pub mod http;

#[cfg(not(target_arch = "wasm32"))]
pub use client::{Executed, NetClient};
