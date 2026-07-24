//! A raw `#[server]`-function call from the browser — the M5 transport
//! smoke.
//!
//! Proves `fetch` POST → node gate → registry dispatch → framed reply end
//! to end without needing a schema crate compiled in: the caller supplies
//! the function's `STRUCT_HASH` and already-wire-encoded args, and the
//! wire-encoded return comes back raw. The typed surface (a real schema
//! crate + `Db`) rides the exact same path.

use wasm_bindgen::prelude::*;
use wavedb_core::U48;
use wavedb_core::expose::{Command, Reply};
use wavedb_net::{Auth, NetClient};

/// Call the `#[server]` function `struct_hash` on the node at `addr`.
///
/// Runs as the anonymous tier under `tenant` (only `#[server(public)]`
/// functions answer it), shipping `payload` as the wire-encoded args tuple.
///
/// # Errors
/// A stringified transport fault, node refusal, or non-function reply.
#[wasm_bindgen]
pub async fn call_fn_raw(
    addr: String,
    tenant: u32,
    struct_hash: u64,
    payload: Vec<u8>,
) -> Result<Vec<u8>, JsValue> {
    let client = NetClient::new(addr);
    let auth = Auth::Anonymous {
        tenant: U48::from(tenant),
    };
    let reply = client
        .call_ok(auth, struct_hash, Command::Get, payload)
        .await
        .map_err(|e| JsValue::from_str(&e.to_string()))?;
    match reply {
        Reply::Returned(bytes) => Ok(bytes),
        _ => Err(JsValue::from_str("not a function reply")),
    }
}
