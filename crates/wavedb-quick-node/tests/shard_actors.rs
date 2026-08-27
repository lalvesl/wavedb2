//! RFC 0064, first slice: shards over one disk actor, end to end.
//!
//! One test function, because the engine claim is process-global — the same
//! constraint `node_http.rs` works under.
//!
//! What is being proven, in order:
//!
//! 1. the index layer runs **unmodified** against `ShardStore` — `Collection`
//!    reaches storage through four `Store` methods and does not know it is now
//!    talking to another thread;
//! 2. the write really reaches the engine rather than sitting in a shard's
//!    cache, observed by a *second* shard whose cache is empty;
//! 3. a shard genuinely runs **on its own thread**, built there because its
//!    state is non-`Send` — only the `DiskHandle` crosses.

// Shard futures are non-`Send` by construction; the test runtime is
// current-thread, which is the model this crate already declares.
#![allow(clippy::future_not_send)]

use std::rc::Rc;

use schema_smoke::{Note, REGISTRY};

use wavedb_core::{LocalHandle, LocalId, U48};
use wavedb_quick_node::shard::{Maintenance, ShardStore, Shards, shard_of};
use wavedb_storage::{PageStore, StorageRegistry};

const TENANT: u32 = 7;

fn note(body: &str) -> Note {
    Note {
        body: body.into(),
        pinned: false,
    }
}

#[tokio::test]
async fn shards_serve_collections_over_one_disk_actor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = PageStore::open(dir.path(), &REGISTRY.storage_entries())
        .expect("open engine");

    // From here the engine is owned by the actor's thread. Nothing in this
    // test can touch it again except by message — which is the point.
    let shards = Shards::start(store, 4).expect("start shards");
    assert_eq!(shards.count(), 4);

    // ---- 1. the index layer, unmodified, over a shard ---------------------
    let first: Rc<ShardStore> = shards.store();
    let db = LocalHandle::new(&*first, U48::from(TENANT));

    let pivot = Note::create_pivot(&db).await.expect("create pivot");
    let col = Note::collection(pivot);
    let mut ids = Vec::new();
    for n in 0..25u32 {
        ids.push(
            col.insert(&db, &note(&format!("note {n}")))
                .await
                .expect("insert"),
        );
    }

    let got = col.get(&db, ids[7]).await.expect("get").expect("present");
    assert_eq!(got.body, "note 7");
    assert!(
        first.cached_len() > 0,
        "a shard that caches nothing is not a shard"
    );

    // ---- 2. the write reached the engine, not just the cache -------------
    // A second `ShardStore` on the same actor, with an empty cache: whatever
    // it can see came back over the wire from the engine. (It shares a Pivot
    // with `first`, which routing would normally forbid — here it is a
    // read-only probe of what actually landed, and nothing writes through it.)
    let observer: Rc<ShardStore> = shards.store();
    assert_eq!(observer.cached_len(), 0, "a fresh shard starts cold");

    let cold_db = LocalHandle::new(&*observer, U48::from(TENANT));
    let cold = Note::collection(pivot);
    let seen = cold
        .get(&cold_db, ids[7])
        .await
        .expect("cold get")
        .expect("the record must have reached the engine");
    assert_eq!(
        seen.body, "note 7",
        "the first shard's cache was answering for a write that never landed"
    );
    assert!(
        observer.cached_bytes() > 0,
        "the cold read should have been memoised"
    );

    // ---- 3. a shard on a thread of its own -------------------------------
    // The factory is `Send` and captures only the handle; `ShardStore` is
    // built on the far side, because it must not be movable between threads.
    let handle = shards.handle();
    let probe = ids[7];
    let (done, wait) = std::sync::mpsc::channel::<Result<String, String>>();
    wavedb_platform::task::spawn_detached("shard-test", move || async move {
        let own = Rc::new(ShardStore::new(handle));
        let db = LocalHandle::new(&*own, U48::from(TENANT));
        let col = Note::collection(pivot);
        let answer = match col.get(&db, probe).await {
            Ok(Some(v)) => Ok(v.body),
            Ok(None) => Err("absent on the far thread".into()),
            Err(e) => Err(format!("{e}")),
        };
        let _ = done.send(answer);
    })
    .expect("spawn shard thread");

    let from_other_thread = wait
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the shard thread must answer")
        .expect("read on the far thread");
    assert_eq!(from_other_thread, "note 7");

    // ---- routing is a function of the pivot, not of anything ambient -----
    assert_eq!(shard_of(pivot.0, 4), shard_of(pivot.0, 4));
    assert!(shard_of(pivot.0, 4) < 4);
    // Two collections of one type are separate owners — the whole premise.
    let second_pivot = Note::create_pivot(&db).await.expect("second pivot");
    assert_ne!(pivot, second_pivot);
    let spread: std::collections::HashSet<usize> = [pivot, second_pivot]
        .iter()
        .map(|p| shard_of(p.0, 4))
        .collect();
    assert!(!spread.is_empty());

    // A Pivot minted here is a `LocalId`, so routing needs nothing but it.
    let _: LocalId = pivot.0;

    for n in 0..40u32 {
        col.insert(&db, &note(&format!("more {n}")))
            .await
            .expect("insert");
    }

    // ---- 4. the low-priority class reaches the actor and runs ------------
    // Scope, stated because an earlier version of this claimed more than it
    // proved: this shows a `Maintenance` hint travelling the second queue and
    // being executed, observed through `EngineStats` (which has to be a
    // message — once the actor owns the engine, "is anything unsettled?" is
    // not answerable by looking).
    //
    // It does **not** prove starvation-freedom. Checked by replacing the valve
    // with strict priority: this still passed, because each `get` completes
    // and leaves the actor idle, so the maintenance step never has to win a
    // turn against anything. Real contention here would need many concurrent
    // reads and would be timing-dependent; the deterministic proof lives in
    // `priority::tests::maintenance_is_never_starved_under_constant_read_pressure`,
    // which strict priority fails outright.
    let before = first.engine_stats().await.expect("stats");
    assert!(before.pending, "40 inserts must leave a settle queued");

    shards.hint(Maintenance::Checkpoint);

    let mut settled = false;
    for _ in 0..200 {
        col.get(&db, ids[3]).await.expect("read");
        if !first.engine_stats().await.expect("stats").pending {
            settled = true;
            break;
        }
    }
    assert!(settled, "the maintenance hint never reached the engine");

    // Concurrent operations on one collection are **not** exercised here, and
    // that is deliberate: `ShardStore` genuinely suspends, so two interleaved
    // collection ops can lose an index update
    // (`wavedb-core/tests/concurrent_node_clobber.rs` states the invariant).
    // What prevents it is `shard::OwnerLocks`, tested at the unit level; a
    // probe here passed one interleaving and proved nothing, so it is gone
    // rather than left looking like a guarantee.
}
