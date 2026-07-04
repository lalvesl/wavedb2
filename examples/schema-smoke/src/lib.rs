//! M1 smoke: what the `#[wavedb]` derive alone guarantees, proven end-to-end
//! without any node, transport, or `Db` — `STRUCT_HASH` identity, `WaveWire`
//! round-trips, shape consts, the generated NonUnique collection types, and
//! the **exposure declarations** (`expose_server!` / `expose_client!`): the
//! lists ARE the registry, and only listed items are dispatchable.

use wavedb_macros::{expose_client, expose_server, wavedb};

// ── Exposure: what each side actually serves / can call ──────────────────────
//
// Entries are plain Rust paths (submodule items work — no scanner). `Invoice`
// hardens its surface: `save` swaps to an audited reimplementation inside the
// match arm at expansion time, `get` is excluded — a `get` command for it
// fails as an unknown hash, indistinguishable from a type that never existed.
expose_server! {
    AboutUser,
    Note,
    billing::Invoice { save: audited_invoice_save, get: never },
}

expose_client! { AboutUser, Note }

/// A hardened per-op override — same signature as the generated step; the
/// exposure arm calls this path instead (compiler-resolved, no callback).
// Store-generic seam — Send only when the backing store is (workspace stance).
#[allow(clippy::future_not_send)]
async fn audited_invoice_save<S: wavedb_core::Store>(
    store: &S,
    tenant: wavedb_core::U48,
    payload: &[u8],
) -> wavedb_core::Result<wavedb_core::expose::Reply> {
    AUDITED_SAVES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    billing::Invoice::__wavedb_save(store, tenant, payload).await
}

/// How many saves went through the audit override (test observability).
static AUDITED_SAVES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

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

/// Already-compressed payloads opt their pages out of zstd — storage policy
/// declared on the type, not schema identity (the hash ignores it).
#[wavedb(compress = false)]
#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct Attachment {
    pub media: Vec<u8>,
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

    // Native targets get compile-time storage: each type carries its own
    // `StructStorage` static (cache + directory, own locks) plus the
    // `storage_entries()` registry list — no runtime STRUCT_HASH map.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_storage_statics_are_per_type() {
        // One slot per type, stamped with that type's own hash.
        assert_eq!(
            AboutUser::struct_storage().struct_hash(),
            AboutUser::STRUCT_HASH
        );
        assert_eq!(Note::struct_storage().struct_hash(), Note::STRUCT_HASH);
        assert!(!std::ptr::eq(
            AboutUser::struct_storage(),
            Note::struct_storage()
        ));

        // The named accessors reach the same static's parts.
        assert!(std::ptr::eq(
            Note::storage_mem_cache(),
            Note::struct_storage().mem_cache()
        ));
        assert!(std::ptr::eq(
            Note::storage_directory(),
            Note::struct_storage().directory()
        ));
        assert!(std::ptr::eq(
            Note::storage_dictionary(),
            Note::struct_storage().dictionary()
        ));

        // Compression is per-type policy: on by default, opted out at the
        // declaration (`#[wavedb(compress = false)]`).
        assert!(Note::struct_storage().compress());
        assert!(!crate::Attachment::struct_storage().compress());

        // Unique registers itself; NonUnique bundles its Pivot's slot too.
        assert_eq!(AboutUser::storage_entries().len(), 1);
        let entries = Note::storage_entries();
        assert_eq!(entries.len(), 2);
        assert!(std::ptr::eq(entries[0], Note::struct_storage()));
        assert!(std::ptr::eq(entries[1], NotePivot::struct_storage()));
    }

    /// `true` when a dispatch refused as an unknown hash.
    fn unknown(r: &wavedb_core::Result<wavedb_core::expose::Reply>) -> bool {
        matches!(r, Err(wavedb_core::Error::UnknownStructHash(_)))
    }

    // Exposure reachability + refusals: the lists are the registry; an
    // unlisted type, a wrong-shape command, and an excluded op all refuse
    // uniformly as an unknown hash.
    #[test]
    fn exposure_reachability_and_uniform_refusals() {
        use futures::executor::block_on;
        use wavedb_core::U48;
        use wavedb_core::expose::{Command, Exposure as _};
        use wavedb_core::wire::to_wire;

        use super::REGISTRY;

        block_on(async {
            let store = mem::MemStore::default();
            let tenant = U48::from(9u32);

            // Reachability is exactly the list.
            assert!(REGISTRY.knows(AboutUser::STRUCT_HASH));
            assert!(REGISTRY.knows(Invoice::STRUCT_HASH));
            assert!(!REGISTRY.knows(0xDEAD_BEEF));
            assert!(
                !REGISTRY.knows(super::Attachment::STRUCT_HASH),
                "unlisted"
            );

            // Wire gate: bodies must decode as the declared type.
            let ada = AboutUser {
                name: "Ada".into(),
                city: "London".into(),
            };
            assert!(
                REGISTRY
                    .decode_check(AboutUser::STRUCT_HASH, &to_wire(&ada))
                    .is_ok()
            );
            assert!(
                REGISTRY
                    .decode_check(AboutUser::STRUCT_HASH, &[1, 2, 3])
                    .is_err()
            );

            // Unlisted hash, wrong-shape command, and excluded op.
            assert!(unknown(
                &REGISTRY
                    .execute(&store, tenant, 0xDEAD_BEEF, Command::Get, &[])
                    .await
            ));
            assert!(unknown(
                &REGISTRY
                    .execute(
                        &store,
                        tenant,
                        AboutUser::STRUCT_HASH,
                        Command::Insert,
                        &[],
                    )
                    .await
            ));
            assert!(unknown(
                &REGISTRY
                    .execute(
                        &store,
                        tenant,
                        Invoice::STRUCT_HASH,
                        Command::Get,
                        &[],
                    )
                    .await
            ));
        });
    }

    // Exposure execution: the Unique round-trip, the override arm, and the
    // client registry's engine-less surface.
    #[test]
    fn exposure_dispatch_end_to_end() {
        use futures::executor::block_on;
        use wavedb_core::U48;
        use wavedb_core::expose::{Command, Exposure as _, Reply};
        use wavedb_core::wire::to_wire;

        use super::{CLIENT_REGISTRY, REGISTRY};

        block_on(async {
            let store = mem::MemStore::default();
            let tenant = U48::from(9u32);

            // Unique: Save then Get round-trips through the dispatch.
            let ada = AboutUser {
                name: "Ada".into(),
                city: "London".into(),
            };
            let done = REGISTRY
                .execute(
                    &store,
                    tenant,
                    AboutUser::STRUCT_HASH,
                    Command::Save,
                    &to_wire(&ada),
                )
                .await
                .unwrap();
            assert_eq!(done, Reply::Done);
            let got = REGISTRY
                .execute(
                    &store,
                    tenant,
                    AboutUser::STRUCT_HASH,
                    Command::Get,
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(got, Reply::Value(Some(to_wire(&ada))));

            // The override path serves Invoice saves (audited, then stored).
            let before =
                super::AUDITED_SAVES.load(std::sync::atomic::Ordering::Relaxed);
            REGISTRY
                .execute(
                    &store,
                    tenant,
                    Invoice::STRUCT_HASH,
                    Command::Save,
                    &to_wire(&Invoice { cents: 12 }),
                )
                .await
                .unwrap();
            assert_eq!(
                super::AUDITED_SAVES.load(std::sync::atomic::Ordering::Relaxed),
                before + 1,
                "the arm must route through the override"
            );

            // The client registry only gates reachability — it never executes.
            assert!(CLIENT_REGISTRY.knows(Note::STRUCT_HASH));
            assert!(!CLIENT_REGISTRY.knows(Invoice::STRUCT_HASH));
            assert!(unknown(
                &CLIENT_REGISTRY
                    .execute(
                        &store,
                        tenant,
                        AboutUser::STRUCT_HASH,
                        Command::Get,
                        &[],
                    )
                    .await
            ));
        });
    }

    // The NonUnique command set through the dispatch: Insert mints, Get
    // resolves, Update re-keys through the record's Metadata pivot back-link
    // (no handle in the payload), Remove moves to dead.
    #[test]
    fn exposure_nonunique_commands_drive_the_collection() {
        use futures::TryStreamExt;
        use futures::executor::block_on;
        use wavedb_core::U48;
        use wavedb_core::expose::{Command, Exposure as _, Reply};
        use wavedb_core::wire::to_wire;

        use super::NoteSecondaries as _;
        use super::REGISTRY;

        block_on(async {
            let store = mem::MemStore::default();
            let tenant = U48::from(11u32);
            let pivot = Note::create_pivot(&store, tenant).await.unwrap();
            let col = Note::collection(pivot, tenant);

            // Insert via the wire shape: (pivot LocalId, body).
            let note = Note {
                body: "hi".into(),
                pinned: false,
            };
            let Reply::Inserted(id) = REGISTRY
                .execute(
                    &store,
                    tenant,
                    Note::STRUCT_HASH,
                    Command::Insert,
                    &to_wire(&(pivot.local_id(), note.clone())),
                )
                .await
                .unwrap()
            else {
                panic!("insert must mint an id")
            };

            // Get by id.
            assert_eq!(
                REGISTRY
                    .execute(
                        &store,
                        tenant,
                        Note::STRUCT_HASH,
                        Command::Get,
                        &to_wire(&id),
                    )
                    .await
                    .unwrap(),
                Reply::Value(Some(to_wire(&note)))
            );

            // Update rides the Metadata pivot back-link — and re-keys the
            // `pinned` secondary index.
            let pinned_now = Note {
                body: "hi".into(),
                pinned: true,
            };
            REGISTRY
                .execute(
                    &store,
                    tenant,
                    Note::STRUCT_HASH,
                    Command::Update,
                    &to_wire(&(id, pinned_now)),
                )
                .await
                .unwrap();
            let pinned: Vec<(wavedb_core::Id, Note)> =
                col.by_pinned(&store, &true).try_collect().await.unwrap();
            assert_eq!(
                pinned.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
                vec![id]
            );

            // Remove moves it out of the living walk.
            assert_eq!(
                REGISTRY
                    .execute(
                        &store,
                        tenant,
                        Note::STRUCT_HASH,
                        Command::Remove,
                        &to_wire(&id),
                    )
                    .await
                    .unwrap(),
                Reply::Removed(true)
            );
            let live: Vec<(wavedb_core::Id, Note)> =
                col.all(&store).try_collect().await.unwrap();
            assert!(live.is_empty());
        });
    }

    // The derived Unique surface keeps the timeline: `save` archives the
    // superseded version, the generated `history` walks it newest-first.
    #[test]
    fn derived_unique_history_walks_versions() {
        use futures::TryStreamExt;
        use futures::executor::block_on;
        use wavedb_core::U48;

        block_on(async {
            let store = mem::MemStore::default();
            let tenant = U48::from(3u32);
            for city in ["Rome", "Oslo", "Lima"] {
                AboutUser {
                    name: "Ada".into(),
                    city: city.into(),
                }
                .save(&store, tenant)
                .await
                .unwrap();
            }
            let versions: Vec<(wavedb_core::Metadata, AboutUser)> =
                AboutUser::history(&store, tenant)
                    .try_collect()
                    .await
                    .unwrap();
            assert_eq!(
                versions
                    .iter()
                    .map(|(_, u)| u.city.as_str())
                    .collect::<Vec<_>>(),
                vec!["Lima", "Oslo", "Rome"],
                "newest-first timeline"
            );
            assert_eq!(
                AboutUser::get(&store, tenant).await.unwrap().unwrap().city,
                "Lima"
            );
        });
    }

    // The generated secondary index end-to-end: `#[wavedb::pivot(pinned)]`
    // emits `by_pinned` on the collection handle; insert indexes, save
    // re-keys, remove de-indexes — all through the derived surface.
    #[test]
    fn derived_secondary_index_by_field() {
        use futures::TryStreamExt;
        use futures::executor::block_on;
        use wavedb_core::U48;

        use super::NoteSecondaries as _;

        block_on(async {
            let store = mem::MemStore::default();
            let tenant = U48::from(7u32);
            let notes = Note::create_pivot(&store, tenant).await.unwrap();
            let col = Note::collection(notes, tenant);

            let a = col
                .insert(
                    &store,
                    &Note {
                        body: "keep".into(),
                        pinned: true,
                    },
                )
                .await
                .unwrap();
            let b = col
                .insert(
                    &store,
                    &Note {
                        body: "later".into(),
                        pinned: false,
                    },
                )
                .await
                .unwrap();

            let pinned: Vec<(wavedb_core::Id, Note)> =
                col.by_pinned(&store, &true).try_collect().await.unwrap();
            assert_eq!(
                pinned.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                vec![a]
            );

            // save with a changed indexed field re-keys the record.
            col.save(
                &store,
                b,
                &Note {
                    body: "later".into(),
                    pinned: true,
                },
            )
            .await
            .unwrap();
            let pinned: Vec<(wavedb_core::Id, Note)> =
                col.by_pinned(&store, &true).try_collect().await.unwrap();
            assert_eq!(pinned.len(), 2);

            // remove de-indexes from the secondary too.
            assert!(col.remove(&store, a).await.unwrap());
            let pinned: Vec<(wavedb_core::Id, Note)> =
                col.by_pinned(&store, &true).try_collect().await.unwrap();
            assert_eq!(
                pinned.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
                vec![b]
            );
        });
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
