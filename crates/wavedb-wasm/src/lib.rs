//! `wavedb-wasm` — the browser client crate.
//!
//! > Status: M5 in progress. The platform seam (`wavedb-platform`) landed:
//! > the whole client stack (`wavedb-core`, `wavedb-net`, `wavedb`)
//! > compiles for wasm32-unknown-unknown, timestamps come from
//! > `Date.now()`, entropy from `crypto.getRandomValues`, and the tunnel
//! > speaks browser `fetch`. This crate currently ships one raw
//! > [`probe`] export that anchors that stack for the size tracker and
//! > browser smoke tests. The IndexedDB `Store` and the typed browser
//! > demo (the M5 exit) are the remaining work — see `todo.md`.
//!
//! Native targets compile this crate empty (it exists so
//! `cargo test --workspace` resolves the workspace).

// Browser futures hold `JsValue`s, which are never `Send`. Established
// stance across the workspace.
#![allow(clippy::future_not_send)]

#[cfg(target_arch = "wasm32")]
pub mod probe;
