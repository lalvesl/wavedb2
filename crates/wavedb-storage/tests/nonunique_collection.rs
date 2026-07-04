//! End-to-end M2 proof: a `#[wavedb(NonUnique)]` type driven through its
//! **derived collection API** (`create_pivot` / `collection` / insert / all /
//! save / remove) over the durable [`PageStore`], surviving a reopen — records,
//! index nodes, **and the `Pivot` record** all recovered from the journal.

use futures::TryStreamExt;
use futures::executor::block_on;
use parking_lot::{Mutex, MutexGuard};
use wavedb_core::{Id, U48};
use wavedb_macros::wavedb;
use wavedb_storage::PageStore;

#[wavedb(NonUnique)]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Todo {
    pub title: String,
    pub completed: bool,
}

/// A secondary-indexed type: `by_tag` rides an extra durable B+tree.
#[wavedb(NonUnique)]
#[wavedb::pivot(tag)]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Bookmark {
    pub tag: String,
    pub url: String,
}

const TENANT: u32 = 42;

/// The per-struct storage slots are process-global statics, so only one
/// `PageStore` may live at a time — serialise the tests that open one.
fn engine_gate() -> MutexGuard<'static, ()> {
    static GATE: Mutex<()> = Mutex::new(());
    GATE.lock()
}

/// Open the durable store serving the derived `Todo` slots (record + Pivot).
fn open(path: &std::path::Path) -> PageStore {
    PageStore::open(path, &Todo::storage_entries()).unwrap()
}

fn todo(title: &str) -> Todo {
    Todo {
        title: title.into(),
        completed: false,
    }
}

async fn walk(
    store: &PageStore,
    col: wavedb_core::Collection<Todo>,
) -> Vec<(Id, Todo)> {
    col.all(store).try_collect().await.unwrap()
}

#[test]
fn collection_walk_and_durable_reopen() {
    let _g = engine_gate();
    let dir = tempfile::tempdir().unwrap();
    let tenant = U48::from(TENANT);
    let n = 60usize;

    let pivot = block_on(async {
        let store = open(dir.path());
        let pivot = Todo::create_pivot(&store, tenant).await.unwrap();
        let col = Todo::collection(pivot, tenant);

        let mut ids = Vec::new();
        for i in 0..n {
            let id = col
                .insert(&store, &todo(&format!("task-{i}")))
                .await
                .unwrap();
            ids.push(id);
        }

        let walked = walk(&store, col).await;
        assert_eq!(
            walked.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            ids,
            "collection must walk in insertion (CREATED_AT) order"
        );
        assert_eq!(walked[0].1, todo("task-0"));
        pivot
    });

    // Reopen: data.bin is rebuilt from the journal — the same PivotId still
    // reaches the whole collection (roots come from the recovered Pivot record).
    block_on(async {
        let store = open(dir.path());
        let col = Todo::collection(pivot, tenant);
        let walked = walk(&store, col).await;
        assert_eq!(walked.len(), n, "lost records across reopen");
        assert_eq!(walked.last().unwrap().1, todo(&format!("task-{}", n - 1)));
    });
}

#[test]
fn save_and_remove_survive_reopen() {
    let _g = engine_gate();
    let dir = tempfile::tempdir().unwrap();
    let tenant = U48::from(TENANT);

    let (pivot, kept, dropped) = block_on(async {
        let store = open(dir.path());
        let pivot = Todo::create_pivot(&store, tenant).await.unwrap();
        let col = Todo::collection(pivot, tenant);

        let a = col.insert(&store, &todo("keep")).await.unwrap();
        let b = col.insert(&store, &todo("drop")).await.unwrap();

        // Update in place: the Id is the stable identity.
        let mut done = todo("keep");
        done.completed = true;
        col.save(&store, a, &done).await.unwrap();

        assert!(col.remove(&store, b).await.unwrap());
        assert!(!col.remove(&store, b).await.unwrap(), "already dead");
        (pivot, a, b)
    });

    block_on(async {
        let store = open(dir.path());
        let col = Todo::collection(pivot, tenant);

        let walked = walk(&store, col).await;
        assert_eq!(
            walked.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![kept],
            "removal must survive reopen"
        );
        assert!(walked[0].1.completed, "update must survive reopen");

        // The dead record's bytes are still resolvable (history navigable).
        assert_eq!(col.get(&store, dropped).await.unwrap(), Some(todo("drop")));
    });
}

fn mark(tag: &str, url: &str) -> Bookmark {
    Bookmark {
        tag: tag.into(),
        url: url.into(),
    }
}

async fn tagged(
    store: &PageStore,
    col: wavedb_core::Collection<Bookmark>,
    tag: &str,
) -> Vec<String> {
    col.by_tag(store, tag)
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .into_iter()
        .map(|(_, b)| b.url)
        .collect()
}

#[test]
fn version_history_survives_reopen() {
    let _g = engine_gate();
    let dir = tempfile::tempdir().unwrap();
    let tenant = U48::from(TENANT);

    let (pivot, id) = block_on(async {
        let store = open(dir.path());
        let pivot = Todo::create_pivot(&store, tenant).await.unwrap();
        let col = Todo::collection(pivot, tenant);

        let id = col.insert(&store, &todo("v1")).await.unwrap();
        col.save(&store, id, &todo("v2")).await.unwrap();
        col.save(&store, id, &todo("v3")).await.unwrap();
        (pivot, id)
    });

    // The archived versions and their chain links are ordinary journaled
    // writes — a rebuild from the log must reproduce the whole timeline.
    block_on(async {
        let store = open(dir.path());
        let col = Todo::collection(pivot, tenant);
        let versions: Vec<(wavedb_core::Metadata, Todo)> =
            col.history(&store, id).try_collect().await.unwrap();
        assert_eq!(
            versions
                .iter()
                .map(|(_, t)| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["v3", "v2", "v1"],
            "history must survive reopen, newest-first"
        );
        // The live walk still sees exactly one record.
        assert_eq!(walk(&store, col).await.len(), 1);
    });
}

#[test]
fn secondary_index_survives_reopen() {
    let _g = engine_gate();
    let dir = tempfile::tempdir().unwrap();
    let tenant = U48::from(TENANT);

    let pivot = block_on(async {
        let store =
            PageStore::open(dir.path(), &Bookmark::storage_entries()).unwrap();
        let pivot = Bookmark::create_pivot(&store, tenant).await.unwrap();
        let col = Bookmark::collection(pivot, tenant);

        let a = col.insert(&store, &mark("rust", "a")).await.unwrap();
        col.insert(&store, &mark("db", "b")).await.unwrap();
        let c = col.insert(&store, &mark("rust", "c")).await.unwrap();

        assert_eq!(tagged(&store, col, "rust").await, vec!["a", "c"]);

        // Re-key one record, drop another — both must survive replay.
        col.save(&store, a, &mark("db", "a")).await.unwrap();
        assert!(col.remove(&store, c).await.unwrap());
        pivot
    });

    // Reopen: the secondary tree is rebuilt from the journal like everything
    // else; the same PivotId reaches it through the recovered Pivot record.
    block_on(async {
        let store =
            PageStore::open(dir.path(), &Bookmark::storage_entries()).unwrap();
        let col = Bookmark::collection(pivot, tenant);
        assert_eq!(
            tagged(&store, col, "rust").await,
            Vec::<String>::new(),
            "re-key + removal must survive reopen"
        );
        // Equal field values order by the records' CREATED_AT (`a` was
        // minted before `b`; its re-key kept its identity).
        assert_eq!(tagged(&store, col, "db").await, vec!["a", "b"]);
    });
}
