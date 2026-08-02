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

/// Resolve the current shape `T` from `base` (a holder-stored id whose `SALT` is a
/// base), walking the version chain until a shape is found on disk and lifting it
/// forward (RFC 0040 §3/§3.1).
///
/// Probes `T`'s derived slot first (`prefer_current`); on a miss it recurses to
/// `T::Prev` and applies [`UpgradeFrom`] on the way back up (`upgrade_on_miss`). A
/// head mismatch at a probed slot (a 15-bit `SALT` collision) is treated as a miss.
/// Returns `None` only when no version holds the record.
///
/// # Errors
/// Propagates any [`Store`] fault, or a non-collision decode error from
/// [`Versioned::from_stored`].
pub async fn resolve<T, S>(
    store: &S,
    base: LocalId,
    tenant: U48,
) -> Result<Option<T>>
where
    T: UpgradeFrom,
    S: Store,
{
    let addr = base.with_salt(type_salt(T::STRUCT_HASH)).to_id(tenant);
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
    let prev = Box::pin(resolve::<T::Prev, S>(store, base, tenant)).await?;
    Ok(prev.map(T::upgrade_from))
}

#[cfg(test)]
mod tests {
    use super::{DowngradeFrom, UpgradeFrom, Versioned, resolve};
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
            let got: Option<V3> = resolve(&s, base(), tenant()).await.unwrap();
            assert_eq!(got, Some(V3 { seed: 7, hops: 0 }));
        });
    }

    #[test]
    fn walks_down_and_upgrades_from_v1() {
        futures::executor::block_on(async {
            let s = MemStore::default();
            put(&s, &V1 { seed: 7 }); // only the oldest slot holds data
            let got: Option<V3> = resolve(&s, base(), tenant()).await.unwrap();
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
            let got: Option<V3> = resolve(&s, base(), tenant()).await.unwrap();
            assert_eq!(got, Some(V3 { seed: 5, hops: 1 }));
        });
    }

    #[test]
    fn absent_everywhere_is_none() {
        futures::executor::block_on(async {
            let s = MemStore::default();
            let got: Option<V3> = resolve(&s, base(), tenant()).await.unwrap();
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
            let got: Option<V3> = resolve(&s, base(), tenant()).await.unwrap();
            assert_eq!(got, Some(V3 { seed: 9, hops: 2 }));
        });
    }
}
