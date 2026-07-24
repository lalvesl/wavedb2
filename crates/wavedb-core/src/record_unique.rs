//! The `Unique`-anchor operations — split from [`crate::record`] for the
//! file budget. A `Unique` type's live record sits at a **fixed anchor**
//! (`KEY = STRUCT_HASH`, `FLAG = 1`, `SALT = 0`) under its tenant; `save` is
//! the upsert and chains history exactly like a NonUnique update.

use crate::error::Result;
use crate::id::Id;
use crate::metadata::Metadata;
use crate::record::{decode_record, history_stream, plan_chained_save};
use crate::store::Store;
use crate::u48::U48;

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
    save_unique_as(store, tenant, tenant, value).await
}

/// [`save_unique`] with authorship: `Metadata.user` is stamped `user` (the
/// verified caller, M8) instead of the tenant.
///
/// # Errors
/// As [`save_unique`].
pub async fn save_unique_as<T, S>(
    store: &S,
    tenant: U48,
    user: U48,
    value: &T,
) -> Result<()>
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
        user,
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
