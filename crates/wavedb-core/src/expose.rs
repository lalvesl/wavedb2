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

/// The 15-bit identity guard the `expose_*` macros instantiate — one call per
/// declared pair, at compile time.
///
/// `DISTINCT` is the const-evaluated verdict for a pair of exposed types: `true`
/// when their [`type_salt`]s differ, `false` when they share one. Only the
/// `false` arm is deprecated, so a clash costs the build a **warning naming the
/// entry**, while a clean registry is silent.
///
/// Sharing the salt is legal — the full 64-bit head still tells the two types
/// apart on read (a full-`STRUCT_HASH` clash is the hard error, asserted
/// alongside this call). It is only a smell worth surfacing: the salt is the
/// archive-slot and flat-keyspace (IndexedDB) discriminator, so a shared value
/// costs those paths their type separation. Rename a field or the type to
/// reshuffle the hash, or keep it knowingly.
///
/// [`type_salt`]: crate::mint::type_salt
pub struct SaltGuard<const DISTINCT: bool>;

impl SaltGuard<true> {
    /// The clean arm — the pair's salts differ, nothing to report.
    pub const fn check() {}
}

impl SaltGuard<false> {
    /// The clashing arm; its deprecation **is** the warning.
    #[deprecated(
        note = "this exposed type shares the low 15 bits of its STRUCT_HASH \
                (`type_salt`) with another entry in the same exposure list: \
                they share archive slots and lose their separation in the \
                browser's flat keyspace. Rename the type or a field to \
                reshuffle the hash, or keep it knowingly."
    )]
    pub const fn check() {}
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
}
