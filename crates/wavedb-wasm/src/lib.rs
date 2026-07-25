//! `wavedb-wasm` — the browser client crate.
//!
//! > Status: M5 complete. The platform seam (`wavedb-platform`) is live —
//! > the whole client stack (`wavedb-core`, `wavedb-net`, `wavedb`)
//! > compiles for wasm32-unknown-unknown, timestamps come from
//! > `Date.now()`, entropy from `crypto.getRandomValues`, and the tunnel
//! > speaks browser `fetch`. The IndexedDB `Store` backend moved into
//! > `wavedb` itself with M6's `Db::open` (it is the browser half of the
//! > client cache); this crate re-exports it as `IdbStore` and ships one
//! > raw `probe` export that anchors the transport stack for the size
//! > tracker. The typed browser demo against a live node is
//! > `tests/live_node.rs`, run through `scripts/browser_demo.sh`.
//! >
//! > Browser-only tests (`tests/idb_store.rs`) need a browser runner:
//! > `wasm-pack test --headless --chrome -p wavedb-wasm`.
//!
//! Native targets compile this crate empty (it exists so
//! `cargo test --workspace` resolves the workspace); the doc references
//! above are plain text because the wasm-only re-exports don't resolve
//! in a native doc build.

// Browser futures hold `JsValue`s, which are never `Send`. Established
// stance across the workspace.
#![allow(clippy::future_not_send)]

#[cfg(target_arch = "wasm32")]
pub mod probe;

#[cfg(target_arch = "wasm32")]
pub use wavedb::IdbStore;
