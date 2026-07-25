//! `wavedb-wasm` — the browser client crate.
//!
//! > Status: M5 in progress. The platform seam (`wavedb-platform`) landed:
//! > the whole client stack (`wavedb-core`, `wavedb-net`, `wavedb`)
//! > compiles for wasm32-unknown-unknown, timestamps come from
//! > `Date.now()`, entropy from `crypto.getRandomValues`, and the tunnel
//! > speaks browser `fetch`. This crate ships the IndexedDB [`Store`]
//! > backend ([`IdbStore`]: flat `Id → wire bytes`, one transaction per
//! > `apply`, no pages, no journal) and one raw [`probe`] export that
//! > anchors the transport stack for the size tracker. The typed browser
//! > demo (the M5 exit) is the remaining work — see `todo.md`.
//! >
//! > Browser-only tests (`tests/idb_store.rs`) need a browser runner:
//! > `wasm-pack test --headless --chrome -p wavedb-wasm`.
//!
//! [`Store`]: wavedb_core::Store
//!
//! Native targets compile this crate empty (it exists so
//! `cargo test --workspace` resolves the workspace).

// Browser futures hold `JsValue`s, which are never `Send`. Established
// stance across the workspace.
#![allow(clippy::future_not_send)]

#[cfg(target_arch = "wasm32")]
mod idb;
#[cfg(target_arch = "wasm32")]
pub mod probe;
#[cfg(target_arch = "wasm32")]
pub mod store;

#[cfg(target_arch = "wasm32")]
pub use store::IdbStore;
