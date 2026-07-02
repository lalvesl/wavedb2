//! End-to-end M2 proof: a `#[wavedb(NonUnique)]` type driven through its
//! **derived collection API** (`create_pivot` / `collection` / insert / all /
//! save / remove) over the durable [`PageStore`], surviving a reopen — records,
//! index nodes, **and the `Pivot` record** all recovered from the journal.

use futures::TryStreamExt;
use futures::executor::block_on;
use wavedb_core::{Id, U48};
use wavedb_macros::wavedb;
use wavedb_storage::PageStore;

#[wavedb(NonUnique)]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Todo {
    pub title: String,
    pub completed: bool,
}

const TENANT: u32 = 42;

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
    let dir = tempfile::tempdir().unwrap();
    let tenant = U48::from(TENANT);
    let n = 60usize;

    let pivot = block_on(async {
        let store = PageStore::open(dir.path()).unwrap();
        let pivot = Todo::create_pivot(&store, tenant).await.unwrap();
        let col = Todo::collection(pivot, tenant);

        let mut ids = Vec::new();
        for i in 0..n {
            let id =
                col.insert(&store, &todo(&format!("task-{i}"))).await.unwrap();
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
        let store = PageStore::open(dir.path()).unwrap();
        let col = Todo::collection(pivot, tenant);
        let walked = walk(&store, col).await;
        assert_eq!(walked.len(), n, "lost records across reopen");
        assert_eq!(walked.last().unwrap().1, todo(&format!("task-{}", n - 1)));
    });
}

#[test]
fn save_and_remove_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let tenant = U48::from(TENANT);

    let (pivot, kept, dropped) = block_on(async {
        let store = PageStore::open(dir.path()).unwrap();
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
        let store = PageStore::open(dir.path()).unwrap();
        let col = Todo::collection(pivot, tenant);

        let walked = walk(&store, col).await;
        assert_eq!(
            walked.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![kept],
            "removal must survive reopen"
        );
        assert!(walked[0].1.completed, "update must survive reopen");

        // The dead record's bytes are still resolvable (history navigable).
        assert_eq!(
            col.get(&store, dropped).await.unwrap(),
            Some(todo("drop"))
        );
    });
}
