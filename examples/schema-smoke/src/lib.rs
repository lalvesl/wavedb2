//! M1 smoke: what the `#[wavedb]` derive alone guarantees, proven end-to-end
//! without any node, transport, or `Db` — `STRUCT_HASH` identity, `WaveWire`
//! round-trips, shape consts, and the generated NonUnique collection types.
//!
//! The former `build.rs` + `include!` registry (`wavedb-build`) is removed;
//! wire reachability will come from the explicit `expose_server!` /
//! `expose_client!` declarations once those land.

use wavedb_macros::wavedb;

/// Unique: one live record per tenant.
#[wavedb]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct AboutUser {
    pub name: String,
    pub city: String,
}

/// NonUnique: many per tenant, with a secondary index on `pinned`.
#[wavedb(NonUnique)]
#[wavedb::pivot(pinned)]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Note {
    pub body: String,
    pub pinned: bool,
}

/// A struct in a submodule — items are named by path, not found by a scanner.
pub mod billing {
    use wavedb_macros::wavedb;

    #[wavedb]
    #[derive(Debug, PartialEq, Eq, Clone, Default)]
    pub struct Invoice {
        pub cents: u64,
    }
}

#[cfg(test)]
mod tests {
    use super::billing::Invoice;
    use super::{AboutUser, Note, NotePivot, NotePivotId};
    use wavedb_core::traits::Shape;
    use wavedb_core::wire::{from_wire, to_wire};
    use wavedb_core::{LocalId, WaveDbStruct};

    // Every declared struct round-trips through its derive-emitted WaveWire
    // impl, and its STRUCT_HASH is a distinct compile-time const.
    #[test]
    fn derived_structs_roundtrip_and_hashes_differ() {
        let about = AboutUser {
            name: "Ada".into(),
            city: "London".into(),
        };
        let note = Note {
            body: "hi".into(),
            pinned: true,
        };
        let invoice = Invoice { cents: 42 };

        assert_eq!(from_wire::<AboutUser>(&to_wire(&about)), Ok(about));
        assert_eq!(from_wire::<Note>(&to_wire(&note)), Ok(note));
        assert_eq!(from_wire::<Invoice>(&to_wire(&invoice)), Ok(invoice));

        assert_ne!(AboutUser::STRUCT_HASH, Note::STRUCT_HASH);
        assert_ne!(AboutUser::STRUCT_HASH, Invoice::STRUCT_HASH);
        assert_ne!(Note::STRUCT_HASH, Invoice::STRUCT_HASH);
    }

    // Shape is a compile-time `const` on the type — no runtime lookup.
    #[test]
    fn shape_is_a_const_not_a_lookup() {
        assert_eq!(AboutUser::SHAPE, Shape::Unique);
        assert_eq!(Note::SHAPE, Shape::NonUnique);
        assert_eq!(Invoice::SHAPE, Shape::Unique);
    }

    // The NonUnique derive emits the collection machinery: a typed PivotId
    // handle and a Pivot with current/dead roots plus one secondary slot per
    // `#[wavedb::pivot(...)]`.
    #[test]
    fn nonunique_generates_pivot_types() {
        let pivot = NotePivot {
            current: LocalId::new(10, false, 1),
            dead: LocalId::new(20, false, 2),
            ..NotePivot::default()
        };
        assert_eq!(pivot.secondaries.len(), 1, "one #[wavedb::pivot(...)]");
        assert_eq!(from_wire::<NotePivot>(&to_wire(&pivot)), Ok(pivot));

        // The typed handle is what a holder stores to reference the collection.
        let handle: <Note as WaveDbStruct>::PivotId =
            NotePivotId::new(LocalId::new(7, false, 0));
        assert_eq!(handle.local_id(), LocalId::new(7, false, 0));
    }

    /// A minimal in-memory `Store` — the whole backend contract the derived
    /// API needs (`get` + atomic `apply`).
    mod mem {
        use std::collections::BTreeMap;
        use std::sync::Mutex;
        use wavedb_core::{Id, Result, Store, Write};

        #[derive(Default)]
        pub struct MemStore(Mutex<BTreeMap<u128, Vec<u8>>>);

        impl Store for MemStore {
            async fn get(&self, id: Id) -> Result<Option<Vec<u8>>> {
                Ok(self.0.lock().unwrap().get(&id.raw()).cloned())
            }
            async fn apply(&self, batch: &[Write]) -> Result<()> {
                let mut m = self.0.lock().unwrap();
                for w in batch {
                    match w {
                        Write::Put(id, b) => {
                            m.insert(id.raw(), b.clone());
                        }
                        Write::Remove(id) => {
                            m.remove(&id.raw());
                        }
                    }
                }
                drop(m);
                Ok(())
            }
        }
    }

    // The generated API end-to-end, in the exact shape application code uses:
    // an explicit `create_pivot`, then `collection(...)` driving
    // insert / all / save / remove — no raw `BpTree` anywhere.
    #[test]
    fn derived_collection_flow_end_to_end() {
        use futures::TryStreamExt;
        use futures::executor::block_on;
        use wavedb_core::U48;

        block_on(async {
            let store = mem::MemStore::default();
            let tenant = U48::from(42u32);

            // A Unique record holds the collection handle (the owning record).
            let notes = Note::create_pivot(&store, tenant).await.unwrap();
            let about = AboutUser {
                name: "Ada".into(),
                city: "London".into(),
            };
            about.save(&store, tenant).await.unwrap();
            assert_eq!(
                AboutUser::get(&store, tenant).await.unwrap(),
                Some(about)
            );

            // Drive the collection through the typed handle.
            let col = Note::collection(notes, tenant);
            let a = col
                .insert(
                    &store,
                    &Note {
                        body: "first".into(),
                        pinned: false,
                    },
                )
                .await
                .unwrap();
            let b = col
                .insert(
                    &store,
                    &Note {
                        body: "second".into(),
                        pinned: true,
                    },
                )
                .await
                .unwrap();

            let walked: Vec<(wavedb_core::Id, Note)> =
                col.all(&store).try_collect().await.unwrap();
            assert_eq!(
                walked.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                vec![a, b],
                "insertion order"
            );

            // Update = save at the stable Id; identity never changes.
            let mut second = walked[1].1.clone();
            second.pinned = false;
            col.save(&store, b, &second).await.unwrap();
            assert_eq!(col.get(&store, b).await.unwrap(), Some(second));

            // Remove drops it from the walk; bytes stay (history).
            assert!(col.remove(&store, a).await.unwrap());
            let after: Vec<(wavedb_core::Id, Note)> =
                col.all(&store).try_collect().await.unwrap();
            assert_eq!(after.len(), 1);
            assert_eq!(after[0].0, b);
            assert!(col.get(&store, a).await.unwrap().is_some());
        });
    }
}
