//! The cache's honesty rules, provable without any node: against a dead
//! address, a **cold** cache propagates the transport fault (absence is not
//! an answer — never an authoritative-looking `None`), while a **warm** one
//! answers; `db.local()` is the direct typed surface that warms it.
//!
//! One test per file: the native cache is the process-global engine
//! (one open `PageStore` per process).

#![allow(clippy::future_not_send)]

use schema_smoke::{AboutUser, CLIENT_REGISTRY};
use wavedb::prelude::*;

/// Nothing listens here — reserved-for-documentation range, refused fast.
const DEAD_NODE: &str = "127.0.0.1:9";

#[tokio::test]
async fn cold_cache_propagates_the_fault_and_a_warm_one_answers() {
    let dir = tempfile::tempdir().expect("cache dir");
    let tenant = U48::from(9u32);
    let db =
        Db::open_at(CLIENT_REGISTRY, DEAD_NODE, tenant, tenant, dir.path())
            .await
            .expect("the cache opens without a reachable node");

    // Cold: the read must surface the transport fault, not mint a `None`
    // that looks like "the record does not exist".
    let cold = AboutUser::get(&db).await;
    assert!(
        matches!(cold, Err(wavedb::Error::Transport(_))),
        "a cold cache must not answer: {cold:?}"
    );

    // Warm it through the local typed surface (what mirroring does), then
    // the same read serves the cached value under the same dead node.
    let local = db.local().expect("open_at attached a cache");
    let profile = AboutUser {
        name: "Ada".into(),
        city: "London".into(),
    };
    local.save_unique(&profile).await.expect("local save");
    assert_eq!(AboutUser::get(&db).await.expect("warm read"), Some(profile));

    // Writes stay write-through: no node, no write — cache still warm.
    let refused = AboutUser {
        name: "Grace".into(),
        city: "NYC".into(),
    }
    .save(&db)
    .await;
    assert!(matches!(refused, Err(wavedb::Error::Transport(_))));
    assert_eq!(
        AboutUser::get(&db)
            .await
            .expect("still warm")
            .map(|p| p.city),
        Some(String::from("London")),
        "a refused write must not have touched the cache"
    );
}
