//! The M4 exit proof: the todo-app's whole client flow against a live node —
//! register/login on the system tenant (username registry + `as_tenant`
//! bootstrap), the profile→pivot todo functions on the user tenant, and the
//! data surviving a node restart.
//!
//! One `#[tokio::test]` per process (the engine's storage slots are
//! process-global statics); the node runs on its own thread with its own
//! current-thread runtime, exactly like production.

#![allow(clippy::future_not_send)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use todo_app_schema::{
    REGISTRY, add_todo, all_todos, complete_todo, delete_todo, login, register,
};
use tokio::sync::oneshot;
use wavedb::prelude::*;
use wavedb_quick_node::{Bound, Server};

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

async fn connect(addr: SocketAddr, tenant: U48) -> Db {
    Db::connect(addr.to_string(), tenant, tenant)
        .await
        .expect("connect")
}

/// Collect the streamed walk (each todo arrives as its own frame).
async fn collect_todos(db: &Db) -> Result<Vec<todo_app_schema::Todo>> {
    all_todos(db).try_collect().await
}

fn titles(todos: &[todo_app_schema::Todo]) -> Vec<(&str, bool)> {
    todos
        .iter()
        .map(|t| (t.title.as_str(), t.completed))
        .collect()
}

#[tokio::test]
async fn full_client_flow_and_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_path_buf();

    // ── phase 1: register, login, drive todos ──────────────────────────────
    let node = start(path.clone());
    let sys = connect(node.addr, U48::ZERO).await;

    let tenant_id = register(&sys, "alice".into(), "secret".into())
        .await
        .expect("register");
    // A duplicate username is refused by the secondary-index lookup.
    let dup = register(&sys, "alice".into(), "other".into()).await;
    assert!(dup.is_err(), "duplicate username must refuse");

    let (login_tenant, token) = login(&sys, "alice".into(), "secret".into())
        .await
        .expect("login");
    assert_eq!(login_tenant, tenant_id);
    assert!(!token.is_empty());
    let wrong = login(&sys, "alice".into(), "nope".into()).await;
    assert!(wrong.is_err(), "wrong password must refuse");

    // Reconnect as the assigned tenant (the client flow).
    let db =
        connect(node.addr, U48::try_from(tenant_id).expect("48-bit")).await;

    let id_milk = add_todo(&db, "Buy milk".into()).await.expect("add 1");
    let id_docs = add_todo(&db, "Write docs".into()).await.expect("add 2");
    add_todo(&db, "Read the Rust book".into())
        .await
        .expect("add 3");

    let all = collect_todos(&db).await.expect("all");
    assert_eq!(
        titles(&all),
        vec![
            ("Buy milk", false),
            ("Write docs", false),
            ("Read the Rust book", false),
        ],
        "insertion order"
    );

    complete_todo(&db, id_milk).await.expect("complete");
    delete_todo(&db, id_docs).await.expect("delete");

    let all = collect_todos(&db).await.expect("all after mutate");
    assert_eq!(
        titles(&all),
        vec![("Buy milk", true), ("Read the Rust book", false)],
    );

    // The system tenant sees no todos — tenant spaces never mix.
    let sys_todos = collect_todos(&sys).await;
    assert!(
        sys_todos.is_err(),
        "the system tenant has no profile — the profile→pivot path refuses"
    );

    node.shutdown();

    // ── phase 2: restart — journal replay must reproduce everything ───────
    let node = start(path);
    let sys = connect(node.addr, U48::ZERO).await;

    let (tenant_again, _) = login(&sys, "alice".into(), "secret".into())
        .await
        .expect("login after restart");
    assert_eq!(tenant_again, tenant_id, "username registry survives");

    let db =
        connect(node.addr, U48::try_from(tenant_id).expect("48-bit")).await;
    let all = collect_todos(&db).await.expect("all after restart");
    assert_eq!(
        titles(&all),
        vec![("Buy milk", true), ("Read the Rust book", false)],
        "todos + completion + deletion survive a restart"
    );

    node.shutdown();
}
