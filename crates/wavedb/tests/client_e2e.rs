//! M4 end-to-end: the unified typed surface against a live node.
//!
//! The exact spelling the docs promise — `AboutUser::get(&db)` /
//! `value.save(&db)` for Unique, `Note::collection(pivot)` +
//! `col.insert(&db, v)` for NonUnique — driven over HTTP POST into a real
//! `PageStore`, through the same generated methods engine tests run against a
//! `LocalHandle`. The collection `Pivot` is seeded node-side (`create_pivot`
//! is not wire-reachable; apps bootstrap inside `#[server]` bodies).

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
            let seed =
                wavedb_core::LocalHandle::new(bound.store(), U48::from(TENANT));
            let pivot = Note::create_pivot(&seed).await.expect("seed pivot");
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
async fn typed_surface_drives_a_live_node() {
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

    unique_phase(&db).await;
    nonunique_phase(&db, node.pivot).await;

    // ── ops without a wire command refuse uniformly ────────────────────────
    let err = Note::create_pivot(&db).await.expect_err("must refuse");
    assert!(
        matches!(err, Error::Core(wavedb_core::Error::UnknownStructHash(_))),
        "create_pivot is not wire-reachable: {err}"
    );

    node.shutdown();
}

/// Unique: get (empty) → save → upsert → history over the wire.
async fn unique_phase(db: &Db) {
    assert_eq!(AboutUser::get(db).await.expect("get"), None);

    let mut me = AboutUser {
        name: "Ada".into(),
        city: "London".into(),
    };
    me.save(db).await.expect("save");
    assert_eq!(AboutUser::get(db).await.expect("get"), Some(me.clone()));

    // save is an upsert — a second save overwrites the live record.
    me.city = "Paris".into();
    me.save(db).await.expect("resave");
    assert_eq!(
        AboutUser::get(db).await.expect("get").unwrap().city,
        "Paris"
    );

    // History walks the version chain newest-first (pillar 3) — with each
    // version's metadata riding the wire.
    let versions: Vec<(Metadata, AboutUser)> =
        AboutUser::history(db).try_collect().await.expect("history");
    assert_eq!(
        versions
            .iter()
            .map(|(_, u)| u.city.as_str())
            .collect::<Vec<_>>(),
        vec!["Paris", "London"],
        "timeline newest-first"
    );
    assert!(
        versions[0].0.new_modification_id.is_none(),
        "the live version has no successor"
    );
}

/// NonUnique: insert → get → save(update) → walk → remove.
async fn nonunique_phase(db: &Db, pivot: NotePivotId) {
    let notes = Note::collection(pivot);

    let id = notes
        .insert(
            db,
            &Note {
                body: "buy milk".into(),
                pinned: false,
            },
        )
        .await
        .expect("insert");
    assert_eq!(
        notes.get(db, id).await.expect("get").unwrap().body,
        "buy milk"
    );

    notes
        .save(
            db,
            id,
            &Note {
                body: "buy milk".into(),
                pinned: true,
            },
        )
        .await
        .expect("update");
    assert!(notes.get(db, id).await.expect("get").unwrap().pinned);

    // A second insert, then walk the whole collection in insertion order.
    notes
        .insert(
            db,
            &Note {
                body: "write docs".into(),
                pinned: false,
            },
        )
        .await
        .expect("insert 2");
    let all: Vec<Note> = notes.all(db).try_collect().await.expect("all");
    assert_eq!(
        all.iter().map(|n| n.body.as_str()).collect::<Vec<_>>(),
        vec!["buy milk", "write docs"],
        "collection walk in CREATED_AT order"
    );

    assert!(notes.remove(db, id).await.expect("remove"));
    // Removing again reports it was no longer in the living set.
    assert!(!notes.remove(db, id).await.expect("remove-again"));
    // The walk now yields only the survivor.
    let all: Vec<Note> =
        notes.all(db).try_collect().await.expect("all after remove");
    assert_eq!(
        all.iter().map(|n| n.body.as_str()).collect::<Vec<_>>(),
        vec!["write docs"]
    );
}
