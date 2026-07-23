//! The contact-book flow end-to-end over the wire: bootstrap via the
//! `#[server]` function, then insert / update (`save`) / remove on the
//! NonUnique collection whose pivot the Unique `ContactBook` owns.
//!
//! One `#[tokio::test]` per process (engine slots are process-global
//! statics); the node runs on its own thread, like production.

#![allow(clippy::future_not_send)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use contact_book::{Contact, ContactBook, REGISTRY, contacts_in, open_book};
use tokio::sync::oneshot;
use wavedb::prelude::*;
use wavedb_quick_node::{Bound, Server};

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
                .expect("test dropped");
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
async fn contact_book_flow_over_the_wire() {
    let dir = tempfile::tempdir().expect("tempdir");
    let node = start(dir.path().to_path_buf());

    let db = Db::connect(
        node.addr.to_string(),
        U48::from(TENANT),
        U48::from(TENANT),
    )
    .await
    .expect("connect");

    // ── Bootstrap (server-side): pivot minted, holder saved ──────────────
    open_book(&db, "Ada".into()).await.expect("open_book");
    open_book(&db, "ignored".into()).await.expect("idempotent");
    let book = ContactBook::get(&db).await.expect("get").expect("exists");
    assert_eq!(book.owner, "Ada");

    // ── The typed collection handle, from the Unique holder ──────────────
    let contacts = Contact::collection(book.contacts);

    // Insert: mints the stable Id (identity for the record's whole life).
    let grace = contacts
        .insert(
            &db,
            &Contact {
                name: "Grace".into(),
                phone: "555-0001".into(),
                city: "NYC".into(),
            },
        )
        .await
        .expect("insert grace");
    let alan = contacts
        .insert(
            &db,
            &Contact {
                name: "Alan".into(),
                phone: "555-0002".into(),
                city: "London".into(),
            },
        )
        .await
        .expect("insert alan");

    // Update: save at the same Id — old version archived on the chain,
    // the changed `city` re-keys the secondary index.
    contacts
        .save(
            &db,
            grace,
            &Contact {
                name: "Grace".into(),
                phone: "555-0001".into(),
                city: "London".into(),
            },
        )
        .await
        .expect("update grace");
    assert_eq!(
        contacts.get(&db, grace).await.expect("get").unwrap().city,
        "London"
    );

    // Secondary index reflects the re-key (queried via the `#[server]`
    // function — filtered reads are functions, not a client-side DSL).
    let londoners = contacts_in(&db, "London".into()).await.expect("in London");
    assert_eq!(londoners.len(), 2);
    let nyc = contacts_in(&db, "NYC".into()).await.expect("in NYC");
    assert!(nyc.is_empty(), "old key de-indexed by the update");

    // Remove: out of the living walk; bytes stay (history navigable).
    assert!(contacts.remove(&db, alan).await.expect("remove"));
    assert!(!contacts.remove(&db, alan).await.expect("remove again"));
    let living: Vec<Contact> =
        contacts.all(&db).try_collect().await.expect("all");
    assert_eq!(
        living.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["Grace"]
    );
    // The removed record still resolves by Id.
    assert!(contacts.get(&db, alan).await.expect("get").is_some());

    node.shutdown();
}
