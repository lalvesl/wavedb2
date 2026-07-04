//! M4 end-to-end: the typed `Db` client surface against a live node.
//!
//! Drives the M4 client API — `db.get::<AboutUser>()` / `db.save(&value)` for
//! Unique, `db.collection::<Note>(pivot)` + `insert`/`get`/`save`/`remove` for
//! NonUnique — all over HTTP POST into a real `PageStore`. The collection
//! `Pivot` is seeded node-side (`create_pivot` is not wire-reachable until the
//! `#[server]` layer lands).

// The typed client futures hold `&Db` across awaits (non-Send by design on the
// current-thread test runtime).
#![allow(clippy::future_not_send)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use schema_smoke::{AboutUser, Note, NotePivotId, REGISTRY};
use tokio::sync::oneshot;
use wavedb::prelude::*;
use wavedb_quick_node::{Bound, Server};

const TENANT: u32 = 7;

struct Node {
    addr: SocketAddr,
    pivot: NotePivotId,
    stop: oneshot::Sender<()>,
    thread: thread::JoinHandle<()>,
}

impl Node {
    fn shutdown(self) {
        self.stop.send(()).expect("server still listening");
        self.thread.join().expect("server thread panicked");
    }
}

/// Start a node in `dir`, seed a `Note` collection pivot, return once bound.
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
            let addr = bound.local_addr().expect("local addr");
            let pivot = Note::create_pivot(bound.store(), U48::from(TENANT))
                .await
                .expect("seed pivot");
            info_tx.send((addr, pivot)).expect("test dropped");
            bound
                .run_with_shutdown(async move {
                    let _ = stop_rx.await;
                })
                .await
                .expect("serve");
        });
    });
    let (addr, pivot) = info_rx.recv().expect("server never bound");
    Node {
        addr,
        pivot,
        stop,
        thread,
    }
}

#[tokio::test]
async fn typed_db_surface_drives_a_live_node() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();
    let node = start(path);

    let db = Db::connect(
        node.addr.to_string(),
        U48::from(TENANT),
        U48::from(TENANT),
    )
    .await
    .expect("connect");

    // ── Unique: get (empty) → save → get (present) ────────────────────────
    assert_eq!(db.get::<AboutUser>().await.expect("get"), None);

    let mut me = AboutUser {
        name: "Ada".into(),
        city: "London".into(),
    };
    db.save(&me).await.expect("save");
    assert_eq!(db.get::<AboutUser>().await.expect("get"), Some(me.clone()));

    // save is an upsert — a second save overwrites the live record.
    me.city = "Paris".into();
    db.save(&me).await.expect("resave");
    assert_eq!(
        db.get::<AboutUser>().await.expect("get").unwrap().city,
        "Paris"
    );

    // ── NonUnique: insert → get → save(update) → remove ───────────────────
    let notes = db.collection::<Note>(node.pivot);

    let id = notes
        .insert(Note {
            body: "buy milk".into(),
            pinned: false,
        })
        .await
        .expect("insert");
    assert_eq!(notes.get(id).await.expect("get").unwrap().body, "buy milk");

    notes
        .save(
            id,
            Note {
                body: "buy milk".into(),
                pinned: true,
            },
        )
        .await
        .expect("update");
    assert!(notes.get(id).await.expect("get").unwrap().pinned);

    assert!(notes.remove(id).await.expect("remove"));
    // Removing again reports it was no longer in the living set.
    assert!(!notes.remove(id).await.expect("remove-again"));

    node.shutdown();
}
