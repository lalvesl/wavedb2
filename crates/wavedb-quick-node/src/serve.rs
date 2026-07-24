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

use tokio::io::AsyncWrite;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{LocalSet, spawn_local};
use wavedb_core::Store;
use wavedb_core::expose::{Exposure, Reply};
use wavedb_core::wire::{from_wire, to_wire};
use wavedb_net::frame::{Request, Response, StreamFrame};
use wavedb_net::http;

use crate::dispatch;

/// Serve `store` under `registry` on an already-bound `listener` until either
/// the `shutdown` future resolves or an accept fault. Each connection is
/// handled on its own local task (current-thread, no `Send` bound);
/// `maintenance` runs alongside on the same [`LocalSet`] (cancelled when
/// serving stops).
///
/// # Errors
/// A fatal accept fault (the listener socket broke).
pub async fn run<E, S, F, M>(
    listener: TcpListener,
    registry: E,
    store: Rc<S>,
    secret: [u8; 32],
    maintenance: M,
    shutdown: F,
) -> wavedb_net::Result<()>
where
    E: Exposure + Copy + 'static,
    S: Store + 'static,
    F: Future<Output = ()>,
    M: Future<Output = ()> + 'static,
{
    let local = LocalSet::new();
    local
        .run_until(async move {
            let upkeep = spawn_local(maintenance);
            tokio::pin!(shutdown);
            loop {
                let sock = tokio::select! {
                    accepted = listener.accept() => accepted?.0,
                    () = &mut shutdown => {
                        // Stop maintaining before the caller's final
                        // drain + checkpoint takes over.
                        upkeep.abort();
                        return Ok(());
                    }
                };
                let store = Rc::clone(&store);
                spawn_local(async move {
                    // A per-connection fault is dropped: it never takes the
                    // node down. (No tracing dep yet — silent.)
                    let _ = serve_connection(sock, &registry, &*store, &secret)
                        .await;
                });
            }
        })
        .await
}

/// Read one request, dispatch it, write the framed response.
async fn serve_connection<E, S>(
    mut sock: TcpStream,
    registry: &E,
    store: &S,
    secret: &[u8; 32],
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
            let response =
                dispatch::handle(registry, store, secret, request).await;
            write_response(&mut writer, response).await?;
        }
        // The envelope itself is malformed — a transport-level client error,
        // not a WaveDB refusal (there is no struct_hash to refuse yet).
        Err(_) => http::write_bad_request(&mut writer).await?,
    }
    Ok(())
}

/// Write one response as its frame sequence: a `Values` reply (a walk)
/// unpacks into one `Item` frame per record — flushed as written, so the
/// client streams them — then an `End`; everything else is a bare `End`.
///
/// (The walk itself is still buffered inside `execute` for now; when the
/// engine goes streaming only this seam's producer changes — the wire and
/// the clients already speak frames.)
async fn write_response<W>(
    w: &mut W,
    response: Response,
) -> wavedb_net::Result<()>
where
    W: AsyncWrite + Unpin,
{
    http::write_ok_head(w).await?;
    let end = match response {
        Response::Ok(Reply::Values(entries)) => {
            for entry in entries {
                let item = to_wire(&StreamFrame::Item(entry));
                http::write_frame(w, &item).await?;
            }
            Response::Ok(Reply::Done)
        }
        other => other,
    };
    http::write_frame(w, &to_wire(&StreamFrame::End(end))).await
}
