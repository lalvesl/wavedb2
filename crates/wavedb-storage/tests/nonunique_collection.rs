//! End-to-end M2 proof: a NonUnique collection driven through a [`PageBpTree`]
//! over the durable [`PageStore`], surviving a reopen (records **and** index
//! nodes recovered from the journal).

use futures::StreamExt;
use futures::executor::block_on;
use wavedb_core::{Bound, Id, Store, U48, Write};
use wavedb_storage::{PageBpTree, PageStore};

const TENANT: u32 = 42;
const SH: u64 = 0xAA_BB_CC_DD_EE_FF_00_11;

/// A wire record: `[STRUCT_HASH (8 LE)][body]`.
fn record(body: &[u8]) -> Vec<u8> {
    let mut v = SH.to_le_bytes().to_vec();
    v.extend_from_slice(body);
    v
}

/// A NonUnique record `Id`: timestamp-keyed (`FLAG = 0`) under `TENANT`.
fn rec_id(created_at: u64) -> Id {
    Id::new(created_at, U48::from(TENANT), false, (created_at & 0x7FFF) as u16)
}

/// Insert a record into both the collection's index and the store.
async fn add(store: &PageStore, tree: &mut PageBpTree, created_at: u64, body: &[u8]) {
    let id = rec_id(created_at);
    store.apply(&[Write::Put(id, record(body))]).await.unwrap();
    tree.insert(store, id).await.unwrap();
}

/// Walk the collection in `CREATED_AT` order, resolving each id to its body.
async fn collect(store: &PageStore, tree: &PageBpTree) -> Vec<(u64, Vec<u8>)> {
    let ids: Vec<Id> = tree
        .search(store, Bound::All)
        .map(|r| r.unwrap())
        .collect()
        .await;
    let mut out = Vec::new();
    for id in ids {
        let bytes = store.get(id).await.unwrap().expect("record present");
        out.push((id.key(), bytes[8..].to_vec())); // strip the STRUCT_HASH head
    }
    out
}

#[test]
fn collection_walk_and_durable_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let n = 60u64;

    let root = block_on(async {
        let store = PageStore::open(dir.path()).unwrap();
        let mut tree = PageBpTree::create(&store, U48::from(TENANT)).await.unwrap();

        // Insert in scrambled order; the index must still walk them sorted.
        for i in 0..n {
            let k = (i.wrapping_mul(97) % n) + 1;
            add(&store, &mut tree, k, format!("body-{k}").as_bytes()).await;
        }

        let walked = collect(&store, &tree).await;
        let keys: Vec<u64> = walked.iter().map(|(k, _)| *k).collect();
        let mut expected: Vec<u64> = (1..=n).collect();
        expected.sort_unstable();
        assert_eq!(keys, expected, "collection must walk in CREATED_AT order");
        assert_eq!(walked[0].1, b"body-1");

        tree.root()
    });

    // Reopen: data.bin is rebuilt from the journal — records and BpTree nodes
    // both come back, and the same root still walks the whole collection.
    block_on(async {
        let store = PageStore::open(dir.path()).unwrap();
        let tree = PageBpTree::at(root, U48::from(TENANT));
        let walked = collect(&store, &tree).await;
        assert_eq!(walked.len() as u64, n, "lost records across reopen");
        let keys: Vec<u64> = walked.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, (1..=n).collect::<Vec<_>>());
        assert_eq!(walked.last().unwrap().1, format!("body-{n}").as_bytes());
    });
}

#[test]
fn remove_then_reopen_reflects_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let root = block_on(async {
        let store = PageStore::open(dir.path()).unwrap();
        let mut tree = PageBpTree::create(&store, U48::from(TENANT)).await.unwrap();
        for k in 1..=10u64 {
            add(&store, &mut tree, k, b"x").await;
        }
        // Remove a few from the index and the store.
        for k in [3u64, 7] {
            assert!(tree.remove(&store, rec_id(k)).await.unwrap());
            store.apply(&[Write::Remove(rec_id(k))]).await.unwrap();
        }
        tree.root()
    });

    block_on(async {
        let store = PageStore::open(dir.path()).unwrap();
        let tree = PageBpTree::at(root, U48::from(TENANT));
        let keys: Vec<u64> = tree
            .search(&store, Bound::All)
            .map(|r| r.unwrap().key())
            .collect()
            .await;
        assert_eq!(keys, vec![1, 2, 4, 5, 6, 8, 9, 10], "deletes must survive reopen");
    });
}
