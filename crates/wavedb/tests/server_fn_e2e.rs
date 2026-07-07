//! M4 `#[server]` end-to-end: a function declared once, its body running on
//! the node against the local store, called from the client over the wire.
//!
//! `set_city` / `get_city` take + return `WaveWire` values; their bodies use
//! the **unified generated spelling** (`Profile::get(db)` / `me.save(db)`) —
//! the same call sites a client or an engine test writes.

#![allow(clippy::future_not_send)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use tokio::sync::oneshot;
use wavedb::prelude::*;
use wavedb_quick_node::{Bound, Server};

// ── schema: compiled into node AND client ─────────────────────────────────

/// Unique profile, storage-only for the wire but registered for the engine.
#[wavedb]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Profile {
    pub city: String,
}

/// Set the tenant's city (upsert), returning the stored value.
#[server]
pub async fn set_city(db: &Db, city: String) -> Result<String> {
    let mut me = Profile::get(db).await?.unwrap_or_default();
    me.city = city;
    me.save(db).await?;
    Ok(me.city)
}

/// Read the tenant's city (empty when unset).
#[server]
pub async fn get_city(db: &Db) -> Result<String> {
    let me = Profile::get(db).await?.unwrap_or_default();
    Ok(me.city)
}

// Profile is listed (so its storage slot registers); the functions ride the
// same list as `fn` entries.
wavedb::expose_server! { Profile, fn set_city, fn get_city }
wavedb::expose_client! { fn set_city, fn get_city }

const TENANT: u32 = 5;

struct Node {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    thread: thread::JoinHandle<()>,
}

impl Node {
    fn shutdown(self) {
        self.stop.send(()).expect("server still listening");
        self.thread.join().expect("server thread panicked");
    }
}

fn start(dir: PathBuf) -> Node {
    let (info_tx, info_rx) = mpsc::channel();
    let (stop, stop_rx) = oneshot::channel::<()>();
    let thread = thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let bound: Bound<_> = Server::new(REGISTRY)
                .data_dir(&dir)
                .bind("127.0.0.1:0")
                .await
                .expect("open + bind");
            info_tx
                .send(bound.local_addr().expect("addr"))
                .expect("dropped");
            bound
                .run_with_shutdown(async move {
                    let _ = stop_rx.await;
                })
                .await
                .expect("serve");
        });
    });
    let addr = info_rx.recv().expect("server never bound");
    Node { addr, stop, thread }
}

#[tokio::test]
async fn server_function_runs_on_the_node() {
    let dir = tempfile::tempdir().expect("tempdir");
    let node = start(dir.path().to_path_buf());
    let db = Db::connect(
        node.addr.to_string(),
        U48::from(TENANT),
        U48::from(TENANT),
    )
    .await
    .expect("connect");

    // Empty before any write.
    assert_eq!(get_city(&db).await.expect("get_city"), "");

    // The function body runs node-side, upserts Profile, returns the value.
    let stored = set_city(&db, "Lisbon".into()).await.expect("set_city");
    assert_eq!(stored, "Lisbon");

    // A fresh call sees the persisted state.
    assert_eq!(get_city(&db).await.expect("get_city"), "Lisbon");

    node.shutdown();
}
