//! Schema versioning — the compile-time chain and the read-walk (RFC 0040).
//!
//! A schema type is declared numbered (`Task1`, `Task2`, …) with `pub type Task =
//! TaskN`; `#[wavedb(prev = …)]` links each version to its predecessor through the
//! [`Versioned::Prev`] associated type. [`resolve`] walks that chain — probing each
//! version's derived `SALT` slot, upgrading on the way back up — so a read of the
//! current shape transparently finds and lifts data written at an older shape,
//! with no global migration and no `dyn` table (the recursion monomorphizes into
//! concrete arms).

use crate::error::{Error, Result};
use crate::id::Id;
use crate::local_id::LocalId;
use crate::mint::type_salt;
use crate::store::Store;
use crate::u48::U48;

/// One shape in a type's version chain.
///
/// `#[wavedb(prev = Prev)]` emits this impl; the **first** version terminates the
/// chain with `Prev = Self` and `IS_FIRST = true`.
pub trait Versioned: Sized {
    /// The immediately preceding shape. The first version sets this to `Self`.
    type Prev: UpgradeFrom;
    /// `true` only for the first (oldest) declared version — where the walk stops.
    const IS_FIRST: bool;
    /// The shape's identity, stamped at the head of its stored bytes.
    const STRUCT_HASH: u64;
    /// Decode this shape from its stored envelope, verifying the head is
    /// [`STRUCT_HASH`](Versioned::STRUCT_HASH).
    ///
    /// # Errors
    /// [`Error::UnknownStructHash`] on a head that is not `STRUCT_HASH` — which the
    /// walk reads as a 15-bit `SALT` collision and skips — or a decode failure.
    fn from_stored(bytes: &[u8]) -> Result<Self>;
}

/// Build a version from its immediate predecessor — written by the developer once
/// per non-first version (the first version's impl is the identity terminator).
pub trait UpgradeFrom: Versioned {
    /// Lift a `Self::Prev` value to this shape.
    fn upgrade_from(prev: Self::Prev) -> Self;
}

/// Reduce a version to its immediate predecessor — written by the developer to
/// serve a reader that only knows an older shape (RFC 0040 §4).
pub trait DowngradeFrom: Versioned {
    /// Project this shape down to its `Self::Prev`.
    fn downgrade_from(current: Self) -> Self::Prev;
}

/// Decode a stored **user record** to its value (the record-level walk).
///
/// Drops the within-version [`Metadata`](crate::Metadata) chain — a walk lifts the
/// value alone, and materialising the current version authors fresh metadata. The
/// macro-emitted [`Versioned::from_stored`] for a record type calls this.
///
/// # Errors
/// [`Error::UnknownStructHash`] on a head that is not `hash`, or a wire fault.
pub fn value_from_record<V: crate::wire::WaveWire>(
    hash: u64,
    bytes: &[u8],
) -> Result<V> {
    crate::record::decode_record::<V>(hash, bytes).map(|(_meta, value)| value)
}

/// Decode a stored **bare envelope** (`[hash][wire]` — a Pivot) to its value; the
/// macro-emitted [`Versioned::from_stored`] for a Pivot type calls this.
///
/// # Errors
/// [`Error::UnknownStructHash`] on a head that is not `hash`, or a wire fault.
pub fn value_from_envelope<V: crate::wire::WaveWire>(
    hash: u64,
    bytes: &[u8],
) -> Result<V> {
    crate::record::decode_envelope::<V>(hash, bytes)
}

/// Resolve the current shape `T`, walking the version chain (RFC 0040 §3).
///
/// `addr_of` maps a version's `STRUCT_HASH` to its on-disk slot — the addressing
/// differs by shape (a `Unique` anchor is self-addressed at `KEY = hash`; a
/// NonUnique record / Pivot swaps the 15-bit `SALT`), so the walk itself is one
/// function. Probes `T`'s slot first (`prefer_current`); on a miss it recurses to
/// `T::Prev` and applies [`UpgradeFrom`] on the way back up (`upgrade_on_miss`). A
/// head mismatch at a probed slot (a 15-bit `SALT` collision) is treated as a miss.
/// Returns `None` only when no version holds the record.
///
/// Prefer the shape-specific [`resolve_unique`] / [`resolve_from_base`] wrappers.
///
/// # Errors
/// Propagates any [`Store`] fault, or a non-collision decode error from
/// [`Versioned::from_stored`].
pub async fn resolve<T, S, F>(store: &S, addr_of: &F) -> Result<Option<T>>
where
    T: UpgradeFrom,
    S: Store,
    F: Fn(u64) -> Id,
{
    let addr = addr_of(T::STRUCT_HASH);
    if let Some(bytes) = store.get_of(T::STRUCT_HASH, addr).await? {
        match T::from_stored(&bytes) {
            Ok(value) => return Ok(Some(value)),
            // A foreign head at this slot = SALT collision; fall through and walk.
            Err(Error::UnknownStructHash(_)) => {}
            Err(other) => return Err(other),
        }
    }
    if T::IS_FIRST {
        return Ok(None);
    }
    // Box the recursion: the future would otherwise be infinitely sized (the first
    // version's `Prev = Self` makes it self-referential). One box per chain level.
    let prev = Box::pin(resolve::<T::Prev, S, F>(store, addr_of)).await?;
    Ok(prev.map(T::upgrade_from))
}

/// The version walk for a **Unique** type: each version sits at its own
/// self-addressed anchor (`KEY = STRUCT_HASH`, `FLAG = 1`, `SALT = 0`).
///
/// # Errors
/// As [`resolve`].
pub async fn resolve_unique<T, S>(store: &S, tenant: U48) -> Result<Option<T>>
where
    T: UpgradeFrom,
    S: Store,
{
    resolve::<T, S, _>(store, &|hash| Id::new(hash, tenant, true, 0)).await
}

/// The version walk from a holder-stored `base` id (a NonUnique record or a
/// Pivot): each version's slot swaps the 15-bit `SALT` to `type_salt(hash)`.
///
/// # Errors
/// As [`resolve`].
pub async fn resolve_from_base<T, S>(
    store: &S,
    base: LocalId,
    tenant: U48,
) -> Result<Option<T>>
where
    T: UpgradeFrom,
    S: Store,
{
    resolve::<T, S, _>(store, &|hash| {
        base.with_salt(type_salt(hash)).to_id(tenant)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        DowngradeFrom, UpgradeFrom, Versioned, resolve_from_base,
        resolve_unique,
    };
    use crate::error::Result;
    use crate::id::Id;
    use crate::local_id::LocalId;
    use crate::mint::type_salt;
    use crate::store::{Store, Write};
    use crate::u48::U48;
    use crate::wire::{WaveWire, to_wire};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    // Distinct 15-bit tails so each toy version lands in its own slot.
    const H1: u64 = 0x0000_0000_0000_1001;
    const H2: u64 = 0x0000_0000_0000_2002;
    const H3: u64 = 0x0000_0000_0000_3003;

    #[derive(Debug, PartialEq, WaveWire)]
    struct V1 {
        seed: u32,
    }
    #[derive(Debug, PartialEq, WaveWire)]
    struct V2 {
        seed: u32,
        upgraded: bool,
    }
    #[derive(Debug, PartialEq, WaveWire)]
    struct V3 {
        seed: u32,
        hops: u8,
    }

    impl Versioned for V1 {
        type Prev = Self;
        const IS_FIRST: bool = true;
        const STRUCT_HASH: u64 = H1;
        fn from_stored(b: &[u8]) -> Result<Self> {
            crate::record::decode_envelope::<Self>(H1, b)
        }
    }
    impl UpgradeFrom for V1 {
        fn upgrade_from(prev: Self) -> Self {
            prev // identity terminator
        }
    }

    impl Versioned for V2 {
        type Prev = V1;
        const IS_FIRST: bool = false;
        const STRUCT_HASH: u64 = H2;
        fn from_stored(b: &[u8]) -> Result<Self> {
            crate::record::decode_envelope::<Self>(H2, b)
        }
    }
    impl UpgradeFrom for V2 {
        fn upgrade_from(prev: V1) -> Self {
            Self {
                seed: prev.seed,
                upgraded: true,
            }
        }
    }
    impl DowngradeFrom for V2 {
        fn downgrade_from(cur: Self) -> V1 {
            V1 { seed: cur.seed }
        }
    }

    impl Versioned for V3 {
        type Prev = V2;
        const IS_FIRST: bool = false;
        const STRUCT_HASH: u64 = H3;
        fn from_stored(b: &[u8]) -> Result<Self> {
            crate::record::decode_envelope::<Self>(H3, b)
        }
    }
    impl UpgradeFrom for V3 {
        fn upgrade_from(prev: V2) -> Self {
            Self {
                seed: prev.seed,
                hops: if prev.upgraded { 2 } else { 1 },
            }
        }
    }
    impl DowngradeFrom for V3 {
        fn downgrade_from(cur: Self) -> V2 {
            V2 {
                seed: cur.seed,
                upgraded: cur.hops >= 2,
            }
        }
    }

    #[derive(Default)]
    struct MemStore(Mutex<BTreeMap<u128, Vec<u8>>>);
    impl Store for MemStore {
        async fn get(&self, id: Id) -> Result<Option<Vec<u8>>> {
            Ok(self.0.lock().unwrap().get(&id.raw()).cloned())
        }
        async fn apply(&self, batch: &[Write]) -> Result<()> {
            {
                let mut m = self.0.lock().unwrap();
                for w in batch {
                    match w {
                        Write::Put(id, b) => {
                            m.insert(id.raw(), b.clone());
                        }
                        Write::Remove(id) => {
                            m.remove(&id.raw());
                        }
                        Write::Expect(..) => {}
                    }
                }
            }
            Ok(())
        }
    }

    fn base() -> LocalId {
        LocalId::new(0xABCD_0000, false, 0)
    }
    fn tenant() -> U48 {
        U48::from(1u32)
    }
    fn envelope(hash: u64, wire: &[u8]) -> Vec<u8> {
        let mut b = hash.to_le_bytes().to_vec();
        b.extend_from_slice(wire);
        b
    }
    fn put<V: Versioned + WaveWire>(store: &MemStore, v: &V) {
        let addr = base().with_salt(type_salt(V::STRUCT_HASH)).to_id(tenant());
        let bytes = envelope(V::STRUCT_HASH, &to_wire(v));
        store.0.lock().unwrap().insert(addr.raw(), bytes);
    }

    #[test]
    fn resolves_current_when_present() {
        futures::executor::block_on(async {
            let s = MemStore::default();
            put(&s, &V3 { seed: 7, hops: 0 });
            let got: Option<V3> =
                resolve_from_base(&s, base(), tenant()).await.unwrap();
            assert_eq!(got, Some(V3 { seed: 7, hops: 0 }));
        });
    }

    #[test]
    fn walks_down_and_upgrades_from_v1() {
        futures::executor::block_on(async {
            let s = MemStore::default();
            put(&s, &V1 { seed: 7 }); // only the oldest slot holds data
            let got: Option<V3> =
                resolve_from_base(&s, base(), tenant()).await.unwrap();
            // Chain ran V1 -> V2 (upgraded) -> V3 (hops == 2).
            assert_eq!(got, Some(V3 { seed: 7, hops: 2 }));
        });
    }

    #[test]
    fn walks_down_and_upgrades_from_v2() {
        futures::executor::block_on(async {
            let s = MemStore::default();
            put(
                &s,
                &V2 {
                    seed: 5,
                    upgraded: false,
                },
            );
            let got: Option<V3> =
                resolve_from_base(&s, base(), tenant()).await.unwrap();
            assert_eq!(got, Some(V3 { seed: 5, hops: 1 }));
        });
    }

    #[test]
    fn absent_everywhere_is_none() {
        futures::executor::block_on(async {
            let s = MemStore::default();
            let got: Option<V3> =
                resolve_from_base(&s, base(), tenant()).await.unwrap();
            assert_eq!(got, None);
        });
    }

    #[test]
    fn salt_collision_is_skipped_not_fatal() {
        futures::executor::block_on(async {
            let s = MemStore::default();
            // Foreign bytes (wrong head) squat V3's derived slot; real data at V1.
            let v3slot = base().with_salt(type_salt(H3)).to_id(tenant());
            s.0.lock()
                .unwrap()
                .insert(v3slot.raw(), envelope(0xDEAD, &[1, 2, 3]));
            put(&s, &V1 { seed: 9 });
            let got: Option<V3> =
                resolve_from_base(&s, base(), tenant()).await.unwrap();
            assert_eq!(got, Some(V3 { seed: 9, hops: 2 }));
        });
    }

    #[test]
    fn resolve_unique_walks_self_addressed_anchors() {
        futures::executor::block_on(async {
            let s = MemStore::default();
            // A Unique version's anchor is KEY = its hash, FLAG = 1, SALT = 0 —
            // each version at a different KEY, not a SALT swap.
            let anchor = Id::new(H1, tenant(), true, 0);
            s.0.lock()
                .unwrap()
                .insert(anchor.raw(), envelope(H1, &to_wire(&V1 { seed: 4 })));
            let got: Option<V3> = resolve_unique(&s, tenant()).await.unwrap();
            assert_eq!(got, Some(V3 { seed: 4, hops: 2 }));
        });
    }

    #[test]
    fn value_helpers_decode_both_envelopes() {
        use crate::metadata::Metadata;
        use crate::record::{encode_envelope, encode_record};

        let v = V2 {
            seed: 3,
            upgraded: true,
        };
        // Bare envelope — the Pivot path.
        let bare = encode_envelope(H2, &v);
        assert_eq!(
            super::value_from_envelope::<V2>(H2, &bare).unwrap(),
            V2 {
                seed: 3,
                upgraded: true
            }
        );
        // Record envelope — the user-record path the macro's `from_stored` uses.
        let rec = encode_record(H2, &Metadata::default(), &v);
        assert_eq!(
            super::value_from_record::<V2>(H2, &rec).unwrap(),
            V2 {
                seed: 3,
                upgraded: true
            }
        );
        // A foreign head is `UnknownStructHash` (what the walk reads as a miss).
        assert!(matches!(
            super::value_from_record::<V2>(0xBAD, &rec),
            Err(crate::Error::UnknownStructHash(_))
        ));
    }
}
