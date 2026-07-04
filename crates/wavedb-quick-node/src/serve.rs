//! The accept loop and per-connection handling (native, HTTP POST).
//!
//! One connection = one request/response exchange (the tunnel sends
//! `connection: close`). Connections are handled concurrently on the current
//! thread via a [`LocalSet`] + [`spawn_local`], so no `Send` bound leaks onto
//! the `Store`-generic engine futures (deliberately non-`Send` — an internal
//! node seam, not a public API).
//!
//! [`spawn_local`]: tokio::task::spawn_local

use std::future::Future;
use std::rc::Rc;

use tokio::net::{TcpListener, TcpStream};
use tokio::task::{LocalSet, spawn_local};
use wavedb_core::Store;
use wavedb_core::expose::Exposure;
use wavedb_core::wire::{from_wire, to_wire};
use wavedb_net::frame::Request;
use wavedb_net::http;

use crate::dispatch;

/// Serve `store` under `registry` on an already-bound `listener` until either
/// the `shutdown` future resolves or an accept fault. Each connection is
/// handled on its own local task (current-thread, no `Send` bound).
///
/// # Errors
/// A fatal accept fault (the listener socket broke).
pub async fn run<E, S, F>(
    listener: TcpListener,
    registry: E,
    store: Rc<S>,
    shutdown: F,
) -> wavedb_net::Result<()>
where
    E: Exposure + Copy + 'static,
    S: Store + 'static,
    F: Future<Output = ()>,
{
    let local = LocalSet::new();
    local
        .run_until(async move {
            tokio::pin!(shutdown);
            loop {
                let sock = tokio::select! {
                    accepted = listener.accept() => accepted?.0,
                    () = &mut shutdown => return Ok(()),
                };
                let store = Rc::clone(&store);
                spawn_local(async move {
                    // A per-connection fault is dropped: it never takes the
                    // node down. (No tracing dep yet — silent.)
                    let _ = serve_connection(sock, &registry, &*store).await;
                });
            }
        })
        .await
}

/// Read one request, dispatch it, write the response.
async fn serve_connection<E, S>(
    mut sock: TcpStream,
    registry: &E,
    store: &S,
) -> wavedb_net::Result<()>
where
    E: Exposure,
    S: Store,
{
    let (mut reader, mut writer) = sock.split();
    let Some(body) = http::read_post(&mut reader).await? else {
        return Ok(()); // peer closed without sending — clean.
    };
    match from_wire::<Request>(&body) {
        Ok(request) => {
            let response = dispatch::handle(registry, store, request).await;
            http::write_ok(&mut writer, &to_wire(&response)).await?;
        }
        // The envelope itself is malformed — a transport-level client error,
        // not a WaveDB refusal (there is no struct_hash to refuse yet).
        Err(_) => http::write_bad_request(&mut writer).await?,
    }
    Ok(())
}
