//! Stored-record plumbing shared by the collection layer: the envelopes, id
//! minting, the plan-time [`Overlay`] view, and the `Unique`-anchor
//! operations.
//!
//! ## Envelopes
//!
//! Every stored value starts `[STRUCT_HASH (8 B LE)]` — storage backends route
//! by those first 8 bytes, and decode verifies them, so a stale or foreign
//! `Id` can't silently decode as the wrong type. Two envelope forms follow it:
//!
//! - **bare** (`Pivot` records — pure addressing, no history):
//!   `[STRUCT_HASH][WaveWire bytes]`;
//! - **record** (Unique + NonUnique user data):
//!   `[STRUCT_HASH][meta_len (u32 LE)][WaveWire(Metadata)][WaveWire body]` —
//!   the length prefix splits the two independently-decodable payloads, and
//!   carrying [`Metadata`] is what makes every version chainable.
//!
//! ## The version chain
//!
//! Saving never destroys the old bytes. A save **archives** the superseded
//! version at a freshly minted id and links the chain through `Metadata`:
//! the live record's `old_modification_id` points at the newest archive, each
//! archive's `old_modification_id` at the one before it, and each archive's
//! `new_modification_id` at the archive that superseded it (`None` on the
//! newest archive — its successor is the live record itself). Walk backward
//! from the live record or forward from any archive.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::id::Id;
use crate::local_id::LocalId;
use crate::metadata::Metadata;
use crate::store::{Store, Write};
use crate::u48::U48;
use crate::wire::{WaveWire, from_wire, to_wire};

// The plan-time read view lives in its own module; re-exported so the
// established `crate::record::Overlay` path (collection_write) still resolves.
pub(crate) use crate::overlay::Overlay;

/// Bytes before the wire body: the `STRUCT_HASH` head.
const ENVELOPE_PREFIX: usize = 8;

/// Bytes before a record's `Metadata`: the head plus the `meta_len` slot.
const RECORD_PREFIX: usize = ENVELOPE_PREFIX + 4;

/// Process-wide counter salting minted record ids, so two records minted in
/// the same nanosecond still get distinct ids.
static RECORD_SALT: AtomicU64 = AtomicU64::new(0);

/// Serialise a value as a stored record: `[hash (8 B LE)][WaveWire bytes]`.
pub(crate) fn encode_envelope<V: crate::wire::WaveWire>(
    hash: u64,
    value: &V,
) -> Vec<u8> {
    let mut out = hash.to_le_bytes().to_vec();
    out.extend_from_slice(&to_wire(value));
    out
}

/// Decode a stored record, verifying its `STRUCT_HASH` head first.
///
/// # Errors
/// [`Error::UnknownStructHash`] if the head is not `hash` (or the value is
/// shorter than the head); [`Error::Wire`] if the body fails to decode.
pub(crate) fn decode_envelope<V: crate::wire::WaveWire>(
    hash: u64,
    bytes: &[u8],
) -> Result<V> {
    let head: [u8; ENVELOPE_PREFIX] = bytes
        .get(..ENVELOPE_PREFIX)
        .and_then(|s| s.try_into().ok())
        .ok_or(Error::UnknownStructHash(0))?;
    let got = u64::from_le_bytes(head);
    if got != hash {
        return Err(Error::UnknownStructHash(got));
    }
    Ok(from_wire::<V>(&bytes[ENVELOPE_PREFIX..])?)
}

/// Mint a fresh timestamp-keyed id under `tenant`: `KEY = CREATED_AT` (nanos),
/// `FLAG = 0` (the record namespace), and a per-process counter salt so ids
/// minted in the same nanosecond stay distinct.
pub(crate) fn mint_timestamped_id(tenant: U48) -> Id {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let salt = (RECORD_SALT.fetch_add(1, Ordering::Relaxed) & 0x7FFF) as u16;
    Id::new(nanos, tenant, false, salt)
}

// ---- Record envelope (Metadata-carrying) ----------------------------------------

/// Serialise a user record:
/// `[hash (8 B LE)][meta_len (u32 LE)][Metadata][body]`.
pub(crate) fn encode_record<V: WaveWire>(
    hash: u64,
    meta: &Metadata,
    value: &V,
) -> Vec<u8> {
    encode_record_raw(hash, meta, &to_wire(value))
}

/// [`encode_record`] over already-encoded body bytes — what archiving uses to
/// move a superseded version without decoding it.
pub(crate) fn encode_record_raw(
    hash: u64,
    meta: &Metadata,
    body: &[u8],
) -> Vec<u8> {
    let meta_bytes = to_wire(meta);
    let mut out =
        Vec::with_capacity(RECORD_PREFIX + meta_bytes.len() + body.len());
    out.extend_from_slice(&hash.to_le_bytes());
    out.extend_from_slice(&(meta_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(&meta_bytes);
    out.extend_from_slice(body);
    out
}

/// Split a stored record into its decoded [`Metadata`] and its raw body
/// bytes, verifying the `STRUCT_HASH` head first.
///
/// # Errors
/// [`Error::UnknownStructHash`] on a head mismatch (or a value shorter than
/// the record prefix); [`Error::Wire`] if the metadata fails to decode.
pub(crate) fn split_record(
    hash: u64,
    bytes: &[u8],
) -> Result<(Metadata, &[u8])> {
    let head: [u8; ENVELOPE_PREFIX] = bytes
        .get(..ENVELOPE_PREFIX)
        .and_then(|s| s.try_into().ok())
        .ok_or(Error::UnknownStructHash(0))?;
    let got = u64::from_le_bytes(head);
    if got != hash {
        return Err(Error::UnknownStructHash(got));
    }
    let len_bytes: [u8; 4] = bytes
        .get(ENVELOPE_PREFIX..RECORD_PREFIX)
        .and_then(|s| s.try_into().ok())
        .ok_or(Error::Wire(wavedb_wire::Error::UnexpectedEof))?;
    let meta_end = RECORD_PREFIX
        .checked_add(u32::from_le_bytes(len_bytes) as usize)
        .filter(|&end| end <= bytes.len())
        .ok_or(Error::Wire(wavedb_wire::Error::UnexpectedEof))?;
    let meta = from_wire::<Metadata>(&bytes[RECORD_PREFIX..meta_end])?;
    Ok((meta, &bytes[meta_end..]))
}

/// Decode a stored record into its [`Metadata`] and typed body.
///
/// # Errors
/// As [`split_record`], plus [`Error::Wire`] on an undecodable body.
pub(crate) fn decode_record<V: WaveWire>(
    hash: u64,
    bytes: &[u8],
) -> Result<(Metadata, V)> {
    let (meta, body) = split_record(hash, bytes)?;
    Ok((meta, from_wire::<V>(body)?))
}

// ---- The version chain -----------------------------------------------------------

/// Plan a chained save of `value` at `live_id`: archive the superseded
/// version (when one exists) at a fresh id, repoint the previous archive's
/// forward link at it, and write the new live record — all as `Write`s for
/// one atomic batch. Returns the writes plus the superseded version's
/// decoded state (`None` on a first save) for the caller's own needs
/// (secondary re-keying).
///
/// # Errors
/// Propagates a [`Store`] failure or a decode fault on the existing record
/// or the previous archive.
pub(crate) async fn plan_chained_save<V: WaveWire, S: Store>(
    store: &S,
    hash: u64,
    live_id: Id,
    tenant: U48,
    value: &V,
    pivot_id: Option<LocalId>,
) -> Result<(Vec<Write>, Option<(Metadata, V)>)> {
    let Some(old_bytes) = store.get_of(hash, live_id).await? else {
        // First version: nothing to archive.
        let meta = Metadata {
            pivot_id,
            user: tenant,
            ..Metadata::default()
        };
        let write = Write::Put(live_id, encode_record(hash, &meta, value));
        return Ok((vec![write], None));
    };
    let (old_meta, old_body) = split_record(hash, &old_bytes)?;
    let old_value = from_wire::<V>(old_body)?;

    let archive_id = mint_timestamped_id(tenant);
    let mut writes = Vec::with_capacity(3);

    // The previous newest archive now has a successor: repoint its forward
    // link from "the live record" (None) to the new archive.
    if let Some(prev) = old_meta.old_modification_id {
        let prev_id = prev.to_id(tenant);
        let prev_bytes = store
            .get_of(hash, prev_id)
            .await?
            .ok_or(Error::RecordMissing(prev_id))?;
        let (mut prev_meta, prev_body) = split_record(hash, &prev_bytes)?;
        prev_meta.new_modification_id = Some(LocalId::from_id(archive_id));
        writes.push(Write::Put(
            prev_id,
            encode_record_raw(hash, &prev_meta, prev_body),
        ));
    }

    // Archive the superseded version byte-for-byte (its own chain links kept;
    // forward = None means "my successor is the live record").
    writes.push(Write::Put(
        archive_id,
        encode_record_raw(hash, &old_meta, old_body),
    ));

    // The new live version chains back at the archive; pivot back-link and
    // permission carry forward.
    let live_meta = Metadata {
        old_modification_id: Some(LocalId::from_id(archive_id)),
        new_modification_id: None,
        pivot_id: old_meta.pivot_id,
        user: tenant,
        device_created: 0,
        permission: old_meta.permission.clone(),
    };
    writes.push(Write::Put(live_id, encode_record(hash, &live_meta, value)));

    Ok((writes, Some((old_meta, old_value))))
}

/// Stream a record's versions **newest-first**: the live record, then each
/// archived version following the `old_modification_id` chain.
pub(crate) fn history_stream<'a, V, S>(
    store: &'a S,
    hash: u64,
    live_id: Id,
    tenant: U48,
) -> impl futures::Stream<Item = Result<(Metadata, V)>> + 'a
where
    V: WaveWire + 'a,
    S: Store,
{
    futures::stream::unfold(Some(live_id), move |next| async move {
        let id = next?;
        let bytes = match store.get_of(hash, id).await {
            Ok(Some(b)) => b,
            Ok(None) => return Some((Err(Error::RecordMissing(id)), None)),
            Err(e) => return Some((Err(e), None)),
        };
        match decode_record::<V>(hash, &bytes) {
            Ok((meta, value)) => {
                let older = meta.old_modification_id.map(|l| l.to_id(tenant));
                Some((Ok((meta, value)), older))
            }
            Err(e) => Some((Err(e), None)),
        }
    })
}

// ---- Unique anchors -----------------------------------------------------------

/// The fixed anchor a `Unique` type's live record sits at under `tenant`.
fn unique_anchor<T: crate::traits::WaveDbStruct>(tenant: U48) -> Id {
    Id::new(T::STRUCT_HASH, tenant, true, 0)
}

/// Fetch a `Unique` record from its anchor (`KEY = STRUCT_HASH`, `FLAG = 1`,
/// `SALT = 0`) under `tenant`. `None` = never saved.
///
/// # Errors
/// Propagates a [`Store`] failure or a decode fault.
pub async fn get_unique<T, S>(store: &S, tenant: U48) -> Result<Option<T>>
where
    T: crate::traits::WaveDbStruct,
    S: Store,
{
    let anchor = unique_anchor::<T>(tenant);
    match store.get_of(T::STRUCT_HASH, anchor).await? {
        Some(bytes) => Ok(Some(decode_record(T::STRUCT_HASH, &bytes)?.1)),
        None => Ok(None),
    }
}

/// Save (insert-or-overwrite) a `Unique` record at its anchor under `tenant`.
/// `save` **is** the upsert — `Unique` types have no separate create.
///
/// A save over an existing record archives the superseded version and links
/// the modification chain (the timeline stays walkable via
/// [`unique_history`]); everything commits as one atomic batch.
///
/// # Errors
/// Propagates a [`Store`] failure or a decode fault on the existing record.
pub async fn save_unique<T, S>(store: &S, tenant: U48, value: &T) -> Result<()>
where
    T: crate::traits::WaveDbStruct,
    S: Store,
{
    let anchor = unique_anchor::<T>(tenant);
    let (writes, _old) = plan_chained_save::<T, S>(
        store,
        T::STRUCT_HASH,
        anchor,
        tenant,
        value,
        None,
    )
    .await?;
    store.apply(&writes).await
}

/// Stream a `Unique` record's versions **newest-first** (the live record,
/// then each archived version along the modification chain). Empty when the
/// record was never saved.
pub fn unique_history<'a, T, S>(
    store: &'a S,
    tenant: U48,
) -> impl futures::Stream<Item = Result<(Metadata, T)>> + 'a
where
    T: crate::traits::WaveDbStruct + 'a,
    S: Store,
{
    use futures::StreamExt;
    let anchor = unique_anchor::<T>(tenant);
    futures::stream::once(async move {
        store
            .get_of(T::STRUCT_HASH, anchor)
            .await
            .map(|b| b.is_some())
    })
    .flat_map(move |exists| match exists {
        Ok(true) => {
            history_stream::<T, S>(store, T::STRUCT_HASH, anchor, tenant)
                .left_stream()
        }
        Ok(false) => futures::stream::empty().left_stream().right_stream(),
        Err(e) => futures::stream::once(async move { Err(e) })
            .right_stream()
            .right_stream(),
    })
}
