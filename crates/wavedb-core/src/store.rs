//! `Store` — the backend seam.
//!
//! Key→value over [`Id`] + wire bytes, with an atomic batch. This is the only
//! thing that differs native vs web; the index layer above (`Pivot`/`BpTree`) is
//! written once against this contract.
//!
//! Async, **no concrete I/O** — the page engine ([`wavedb-storage`]), the native
//! client file store, and the browser IndexedDB store each supply their own impl.
//!
//! [`wavedb-storage`]: https://docs.rs/wavedb-storage

use crate::error::Result;
use crate::id::Id;

/// One write inside an atomic [`Store::apply`] batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// Insert or overwrite `Id`'s wire bytes.
    Put(Id, Vec<u8>),
    /// Delete `Id`.
    Remove(Id),
}

impl Write {
    /// The `Id` this write targets.
    #[must_use]
    pub const fn id(&self) -> Id {
        match self {
            Self::Put(id, _) | Self::Remove(id) => *id,
        }
    }
}

/// Key→value backend over [`Id`] plus an **atomic batch**.
///
/// `apply` commits all-or-nothing: a multi-record op (a record **and** the
/// `BpTree` node it touches) is one batch, so a reader never sees a half-applied
/// mutation and a crash either replays the whole batch or none of it. There is no
/// separate transaction manager — the batch *is* the atomic unit.
pub trait Store {
    /// Fetch a record's wire bytes, or `None` if absent.
    async fn get(&self, id: Id) -> Result<Option<Vec<u8>>>;

    /// Apply a batch of writes atomically (all-or-nothing).
    async fn apply(&self, batch: &[Write]) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::{Store, Write};
    use crate::error::Result;
    use crate::id::Id;
    use crate::u48::U48;

    /// In-memory `Store` for exercising the contract and the index layer.
    #[derive(Default)]
    struct MemStore(Mutex<BTreeMap<u128, Vec<u8>>>);

    impl Store for MemStore {
        async fn get(&self, id: Id) -> Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(&id.raw()).cloned())
        }

        async fn apply(&self, batch: &[Write]) -> Result<()> {
            // No await between lock and unlock — the guard never spans a yield.
            {
                let mut map = self.0.lock().unwrap();
                for w in batch {
                    match w {
                        Write::Put(id, bytes) => {
                            map.insert(id.raw(), bytes.clone());
                        }
                        Write::Remove(id) => {
                            map.remove(&id.raw());
                        }
                    }
                }
            }
            Ok(())
        }
    }

    fn id(key: u64) -> Id {
        Id::new(key, U48::from(1u32), false, 0)
    }

    #[test]
    fn apply_is_all_or_nothing_visible() {
        futures::executor::block_on(async {
            let store = MemStore::default();
            assert_eq!(store.get(id(1)).await.unwrap(), None);

            store
                .apply(&[
                    Write::Put(id(1), vec![10, 20]),
                    Write::Put(id(2), vec![30]),
                ])
                .await
                .unwrap();
            assert_eq!(store.get(id(1)).await.unwrap(), Some(vec![10, 20]));
            assert_eq!(store.get(id(2)).await.unwrap(), Some(vec![30]));

            store
                .apply(&[Write::Remove(id(1)), Write::Put(id(2), vec![99])])
                .await
                .unwrap();
            assert_eq!(store.get(id(1)).await.unwrap(), None);
            assert_eq!(store.get(id(2)).await.unwrap(), Some(vec![99]));
        });
    }

    #[test]
    fn write_id_accessor() {
        assert_eq!(Write::Put(id(7), vec![]).id(), id(7));
        assert_eq!(Write::Remove(id(7)).id(), id(7));
    }
}
