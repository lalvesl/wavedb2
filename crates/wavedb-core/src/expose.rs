//! The exposure contract — what `expose_server!` / `expose_client!`
//! expansions implement, and the helpers the generated per-op execution
//! steps call.
//!
//! The registry is **declared, not discovered**: each side lists, in an
//! explicit module, exactly which items it serves or calls, and the macro
//! expands the list into a `match` on the 64-bit `STRUCT_HASH` per operation
//! — concrete, monomorphized arms. No `dyn`, no fn-pointer tables, no runtime
//! registration; an override substitutes a path inside its arm at expansion
//! time.
//!
//! **Every refusal is [`Error::UnknownStructHash`]** — an unlisted type, an
//! excluded (`never`) op, and a command a shape doesn't support are
//! deliberately indistinguishable from a type that never existed (the
//! security surface leaks nothing about what storage holds).

use crate::collection::Collection;
use crate::error::{Error, Result};
use crate::id::Id;
use crate::index::Pivot as _;
use crate::local_id::LocalId;
use crate::record;
use crate::store::Store;
use crate::traits::{NonUniqueStruct, WaveDbStruct};
use crate::u48::U48;
use crate::wire::{WaveWire, from_wire, to_wire};

// The catch-up navigation (the `Changes` command's engine) lives in its own
// module; re-exported so the wire vocabulary reads from one place.
pub use crate::expose_changes::{Change, collection_changes, unique_changes};

/// The wire command set: `Get`/`Save` for a `Unique` type,
/// `Insert`/`Update`/`Remove`/`Get` for a NonUnique one. A `#[server]`
/// function (M4) ignores it — its hash *is* the operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, WaveWire)]
pub enum Command {
    /// Unique: the anchor record. NonUnique: the record at the payload `Id`.
    Get,
    /// Unique upsert (payload = the record body).
    Save,
    /// NonUnique insert (payload = `(PivotId's LocalId, body)`).
    Insert,
    /// NonUnique update at a stable `Id` (payload = `(Id, body)`).
    Update,
    /// NonUnique move to the dead tree (payload = the `Id`).
    Remove,
    /// NonUnique collection walk in `CREATED_AT` order (payload = the
    /// collection's `Pivot` `LocalId`). Buffered for now — streaming frames
    /// are a later transport refinement.
    All,
    /// Unique version-chain walk, newest-first (empty payload → the tenant's
    /// anchor). Buffered like `All`.
    History,
    /// Everything that changed since a cursor — the reconnect catch-up
    /// (payload = `(Option<LocalId> pivot, Option<u64> since)`; `None`
    /// since = register: answer the current tail, no events). Answered as
    /// `Values` of [`Change`] entries, `Cursor` first.
    Changes,
}

/// What an executed command yields. Derives [`WaveWire`] so the transport
/// layer ships it verbatim — the node's reply envelope wraps this value.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub enum Reply {
    /// A `Get`'s result: the record's body wire bytes (`None` = absent).
    Value(Option<Vec<u8>>),
    /// An `Insert`'s minted record identity.
    Inserted(Id),
    /// A `Remove`'s outcome (`false` = was not in the living set).
    Removed(bool),
    /// A `Save`/`Update` completed.
    Done,
    /// An `All` walk's results: each record's body wire bytes, in
    /// `CREATED_AT` order.
    Values(Vec<Vec<u8>>),
    /// A `#[server]` function's wire-encoded return value.
    Returned(Vec<u8>),
}

/// The verified identity a command executes as — gate 1's output.
///
/// `tenant` partitions the data; `user` is authorship (`Metadata.user`).
/// For a B2C session they are equal. The unauthenticated tier is
/// `user == U48::MAX`: only `#[server(public)]` functions accept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    /// The authenticated user (`U48::MAX` = anonymous).
    pub user: U48,
    /// The tenant the command executes under.
    pub tenant: U48,
}

impl Caller {
    /// A caller whose user *is* the tenant — the engine-local/B2C identity.
    #[must_use]
    pub const fn tenant_owned(tenant: U48) -> Self {
        Self {
            user: tenant,
            tenant,
        }
    }

    /// The unauthenticated tier under a claimed tenant.
    #[must_use]
    pub const fn anonymous(tenant: U48) -> Self {
        Self {
            user: U48::MAX,
            tenant,
        }
    }

    /// Is this the unauthenticated tier?
    #[must_use]
    pub const fn is_anonymous(&self) -> bool {
        self.user.get() == U48::MAX.get()
    }
}

/// The declared registry surface.
///
/// Implemented by the zero-sized types `expose_server!` / `expose_client!`
/// emit, and consumed by the node builder (`.registry(REGISTRY)`) via a
/// plain generic bound: static dispatch end to end.
pub trait Exposure {
    /// Is `struct_hash` a declared item on this side? Unlisted ⇒ the wire
    /// cannot name it.
    fn knows(&self, struct_hash: u64) -> bool;

    /// Decode-check `bytes` as the declared type's body — the wire gate that
    /// runs before any engine work.
    ///
    /// # Errors
    /// [`Error::UnknownStructHash`] for an unlisted hash; [`Error::Wire`] on
    /// an undecodable body.
    fn decode_check(&self, struct_hash: u64, bytes: &[u8]) -> Result<()>;

    /// Execute `command` for `struct_hash` over `store` — the engine
    /// dispatch (server side only; the client default refuses).
    ///
    /// # Errors
    /// [`Error::UnknownStructHash`] for anything not declared (or excluded);
    /// otherwise whatever the executed op raises.
    async fn execute<S: Store>(
        &self,
        store: &S,
        caller: Caller,
        struct_hash: u64,
        command: Command,
        payload: &[u8],
    ) -> Result<Reply> {
        let _ = (store, caller, command, payload);
        Err(Error::UnknownStructHash(struct_hash))
    }
}

/// Decode-check `bytes` as a `T` body (the generated `decode_check` arms).
///
/// # Errors
/// [`Error::Wire`] when the bytes are not a valid `T`.
pub fn decode_check<T: WaveDbStruct>(bytes: &[u8]) -> Result<()> {
    from_wire::<T>(bytes)?;
    Ok(())
}

/// Fetch the record at `id` as its body wire bytes — the shared tail of both
/// shapes' `Get` steps.
///
/// # Errors
/// Propagates a [`Store`] failure or a decode fault.
pub async fn get_value<T, S>(store: &S, id: Id) -> Result<Reply>
where
    T: WaveDbStruct,
    S: Store,
{
    match store.get_of(T::STRUCT_HASH, id).await? {
        Some(bytes) => {
            let (_, value) =
                record::decode_record::<T>(T::STRUCT_HASH, &bytes)?;
            Ok(Reply::Value(Some(to_wire(&value))))
        }
        None => Ok(Reply::Value(None)),
    }
}

/// Like [`get_value`] but version-aware for a `Unique` type (RFC 0040).
///
/// Walks the version chain from the current shape's anchor and lifts an older
/// on-disk record forward via [`UpgradeFrom`](crate::version::UpgradeFrom). The
/// wire result is identical — the current shape's value bytes — so a caller
/// never learns a migration happened.
///
/// # Errors
/// As [`crate::version::resolve_unique`].
pub async fn get_unique_versioned<T, S>(store: &S, tenant: U48) -> Result<Reply>
where
    T: crate::version::UpgradeFrom + WaveWire,
    S: Store,
{
    let resolved =
        crate::version::resolve_unique::<T, S>(store, tenant).await?;
    Ok(Reply::Value(resolved.map(|value| to_wire(&value))))
}

/// Walk a NonUnique collection in `CREATED_AT` order and buffer each record
/// as the wire pair `(Id, T)` — the shared tail of the generated `All` step.
///
/// The node-minted `Id` rides with every item so a client can mirror the walk
/// into its local cache under the authoritative identity (M6); the typed
/// surface still yields values only.
///
/// Buffered (not streamed) for now: the HTTP POST tunnel answers one request
/// with one response, so a walk collects before replying. Streaming frames are
/// a later transport refinement.
///
/// # Errors
/// Propagates a [`Store`] failure or a decode fault while walking.
pub async fn all_values<T, S>(
    store: &S,
    pivot: LocalId,
    tenant: U48,
) -> Result<Reply>
where
    T: NonUniqueStruct,
    S: Store,
{
    use futures::TryStreamExt;
    let col = Collection::<T>::at(pivot, tenant);
    // Each entry is the wire triple `(Id, Metadata, T)`: the node-minted
    // identity and the authoritative chain data ride along so a client
    // mirror adopts them verbatim; the typed surface still yields values —
    // hence the direct tree walk here, decoding once and keeping the
    // metadata `all` would drop.
    let pivot_record = col.load_pivot(store).await?;
    let items: Vec<(Id, crate::metadata::Metadata, T)> = col
        .tree(pivot_record.current())
        .search(store, crate::index::Bound::All)
        .and_then(|id| async move {
            let bytes = store
                .get_of(T::STRUCT_HASH, id)
                .await?
                .ok_or(Error::RecordMissing(id))?;
            let (meta, value) =
                crate::record::decode_record::<T>(T::STRUCT_HASH, &bytes)?;
            Ok((id, meta, value))
        })
        .try_collect()
        .await?;
    let entries = items.iter().map(to_wire).collect();
    Ok(Reply::Values(entries))
}

/// Walk a `Unique` record's version chain **newest-first**, buffered.
///
/// Each version rides as the wire tuple `(Metadata, T)` — the shared tail of
/// the generated `History` step. Empty when the record was never saved.
///
/// Buffered like [`all_values`]; each `Values` entry carries the version's
/// metadata alongside its body so a remote timeline walk sees the chain.
///
/// # Errors
/// Propagates a [`Store`] failure or a decode fault while walking.
pub async fn unique_history_values<T, S>(
    store: &S,
    tenant: U48,
) -> Result<Reply>
where
    T: WaveDbStruct,
    S: Store,
{
    use futures::TryStreamExt;
    let versions: Vec<(crate::metadata::Metadata, T)> =
        record::unique_history::<T, S>(store, tenant)
            .try_collect()
            .await?;
    let entries = versions.into_iter().map(|pair| to_wire(&pair)).collect();
    Ok(Reply::Values(entries))
}

/// The owning `Pivot` back-link stamped in the record at `id`'s metadata —
/// how a handle-less `Update`/`Remove` reaches the collection's tree roots.
///
/// # Errors
/// [`Error::RecordMissing`] when `id` resolves to nothing;
/// [`Error::PivotMissing`] when the record carries no back-link (not a
/// collection record).
pub async fn record_pivot<T, S>(store: &S, id: Id) -> Result<LocalId>
where
    T: WaveDbStruct,
    S: Store,
{
    let bytes = store
        .get_of(T::STRUCT_HASH, id)
        .await?
        .ok_or(Error::RecordMissing(id))?;
    let (meta, _) = record::split_record(T::STRUCT_HASH, &bytes)?;
    meta.pivot_id
        .ok_or_else(|| Error::PivotMissing(LocalId::default()))
}

#[cfg(test)]
mod tests {
    use super::{Command, decode_check};
    use crate::wire::{from_wire, to_wire};

    #[test]
    fn command_roundtrips_on_the_wire() {
        for c in [
            Command::Get,
            Command::Save,
            Command::Insert,
            Command::Update,
            Command::Remove,
        ] {
            assert_eq!(from_wire::<Command>(&to_wire(&c)).unwrap(), c);
        }
    }

    #[test]
    fn reply_roundtrips_on_the_wire() {
        use super::Reply;
        use crate::id::Id;
        use crate::u48::U48;

        for r in [
            Reply::Value(None),
            Reply::Value(Some(vec![1, 2, 3])),
            Reply::Inserted(Id::new(7, U48::from(9u32), false, 3)),
            Reply::Removed(true),
            Reply::Done,
        ] {
            assert_eq!(from_wire::<Reply>(&to_wire(&r)).unwrap(), r);
        }
    }

    #[test]
    fn decode_check_gates_bodies() {
        // `u64` isn't a WaveDbStruct; use a unit fixture instead.
        use crate::traits::{Shape, WaveDbStruct};
        use crate::wire::WaveWire;

        #[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
        struct Probe {
            n: u64,
        }
        impl WaveDbStruct for Probe {
            const STRUCT_HASH: u64 = 0xBEEF;
            const SHAPE: Shape = Shape::Unique;
            type PivotId = ();
        }

        assert!(decode_check::<Probe>(&to_wire(&Probe { n: 4 })).is_ok());
        assert!(decode_check::<Probe>(&[1, 2]).is_err());
    }

    // Version-chain fixtures (RFC 0040): a first version `W1` and its successor
    // `W2`, decoding real record envelopes like the macro-emitted impls do.
    mod ver {
        use crate::id::Id;
        use crate::store::{Store, Write};
        use crate::version::{UpgradeFrom, Versioned};
        use crate::wire::WaveWire;
        use std::collections::BTreeMap;
        use std::sync::Mutex;

        pub const W1_HASH: u64 = 0x0000_0000_00AA_0001;
        pub const W2_HASH: u64 = 0x0000_0000_00BB_0002;

        #[derive(Debug, PartialEq, Eq, WaveWire)]
        pub struct W1 {
            pub n: u32,
        }
        #[derive(Debug, PartialEq, Eq, WaveWire)]
        pub struct W2 {
            pub n: u32,
            pub doubled: u32,
        }

        impl Versioned for W1 {
            type Prev = Self;
            const IS_FIRST: bool = true;
            const STRUCT_HASH: u64 = W1_HASH;
            fn from_stored(b: &[u8]) -> crate::Result<Self> {
                crate::version::value_from_record::<Self>(W1_HASH, b)
            }
        }
        impl UpgradeFrom for W1 {
            fn upgrade_from(prev: Self) -> Self {
                prev
            }
        }
        impl Versioned for W2 {
            type Prev = W1;
            const IS_FIRST: bool = false;
            const STRUCT_HASH: u64 = W2_HASH;
            fn from_stored(b: &[u8]) -> crate::Result<Self> {
                crate::version::value_from_record::<Self>(W2_HASH, b)
            }
        }
        impl UpgradeFrom for W2 {
            fn upgrade_from(prev: W1) -> Self {
                Self {
                    n: prev.n,
                    doubled: prev.n * 2,
                }
            }
        }

        #[derive(Default)]
        pub struct MemStore(pub Mutex<BTreeMap<u128, Vec<u8>>>);
        impl Store for MemStore {
            async fn get(&self, id: Id) -> crate::Result<Option<Vec<u8>>> {
                Ok(self.0.lock().unwrap().get(&id.raw()).cloned())
            }
            async fn apply(&self, batch: &[Write]) -> crate::Result<()> {
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
    }

    // A Unique `get` walks the version chain and lifts an older on-disk record
    // forward — the wire result is the current shape's value (RFC 0040 §3).
    #[test]
    fn unique_get_walks_and_upgrades_older_versions() {
        use super::get_unique_versioned;
        use crate::id::Id;
        use crate::metadata::Metadata;
        use crate::record::encode_record;
        use crate::store::{Store, Write};
        use crate::u48::U48;
        use ver::{MemStore, W1, W1_HASH, W2};

        futures::executor::block_on(async {
            let tenant = U48::from(1u32);
            let s = MemStore::default();
            // A v1 record sits at v1's Unique anchor; the current shape is W2.
            let anchor = Id::new(W1_HASH, tenant, true, 0);
            let rec =
                encode_record(W1_HASH, &Metadata::default(), &W1 { n: 21 });
            s.apply(&[Write::Put(anchor, rec)]).await.unwrap();

            let reply =
                get_unique_versioned::<W2, _>(&s, tenant).await.unwrap();
            let super::Reply::Value(Some(wire)) = reply else {
                panic!("expected a value reply");
            };
            assert_eq!(
                from_wire::<W2>(&wire).unwrap(),
                W2 { n: 21, doubled: 42 }
            );
        });
    }
}
