//! **Declared lists** ([RFC 0051]) — `#[wavedb::list(...)]`: one more chain of
//! the same records, kept sorted by a declared property instead of by
//! modification instant.
//!
//! The mechanism is RFC 0050's, unchanged. What a declaration changes is only
//! *which* key the chain is laid out by, so this module is thin on purpose: the
//! write half threads each list through the same `plan_insert`/`plan_remove` the
//! built-in chain uses, and the read half is the same segment walk.
//!
//! Two differences from the built-in chain are load-bearing:
//!
//! - **The tie-break is the anchor**, not the live version's authoring instant.
//!   The anchor never changes, so a record relocates inside a sorted chain only
//!   when the declared property itself changed. In the built-in chain the
//!   instant *is* the key and relocating on every save **is** the mechanism —
//!   here the same choice would be pure cost.
//! - **A list reads ascending**, head to tail, because that is what a declared
//!   order means. The built-in chain reads from the tail because its order is
//!   recency.
//!
//! Liveness still lives on the anchor: a list holds exactly the living records,
//! so every planner here is gated on the built-in chain's verdict rather than
//! deciding membership on its own.
//!
//! [RFC 0051]: https://github.com/wavedb/wavedb/blob/main/rfcs/0051-ordered-record-lists.md

use futures::{Stream, TryStreamExt};

use crate::collection::Collection;
use crate::error::{Error, Result};
use crate::id::Id;
use crate::index::Chain;
use crate::record::{Overlay, decode_record};
use crate::store::{Store, Write};
use crate::traits::NonUniqueStruct;

impl<T: NonUniqueStruct> Collection<T> {
    /// File a freshly inserted record into every declared list.
    ///
    /// Each list is an independent chain with its own segment ids, so they need
    /// no shared overlay between them — only within one chain, which
    /// `plan_insert` already arranges.
    pub(crate) async fn plan_list_inserts<S: Store>(
        &self,
        store: &S,
        batch: &mut Vec<Write>,
        lists: &mut [Chain<Vec<u8>>],
        id: Id,
        envelope: &[u8],
        value: &T,
    ) -> Result<()> {
        for (i, chain) in lists.iter_mut().enumerate() {
            let key = Self::list_key(value, i, id);
            batch.extend(
                chain.plan_insert(store, key, envelope.to_vec()).await?,
            );
        }
        Ok(())
    }

    /// Re-file a saved record in every declared list.
    ///
    /// The record's bytes are duplicated in each list, so **every** list is
    /// rewritten whether or not its property changed — that is the cost RFC 0051
    /// states plainly (a save carries K× the record's bytes). What the
    /// comparison saves is the *removal*: an unchanged property means the entry
    /// stays where it is and `plan_insert` replaces its payload in place, one
    /// segment write instead of two.
    pub(crate) async fn plan_list_moves<S: Store>(
        &self,
        view: &mut Overlay<'_, S>,
        batch: &mut Vec<Write>,
        lists: &mut [Chain<Vec<u8>>],
        id: Id,
        envelope: &[u8],
        values: (&T, &T),
    ) -> Result<()> {
        let (old_value, value) = values;
        for (i, chain) in lists.iter_mut().enumerate() {
            let old_key = Self::list_key(old_value, i, id);
            let new_key = Self::list_key(value, i, id);
            if old_key != new_key
                && let Some(writes) =
                    chain.plan_remove(&*view, &old_key).await?
            {
                view.stage(&writes);
                batch.extend(writes);
            }
            let writes = chain
                .plan_insert(&*view, new_key, envelope.to_vec())
                .await?;
            view.stage(&writes);
            batch.extend(writes);
        }
        Ok(())
    }

    /// Take a removed record out of every declared list.
    ///
    /// A list holds only living records — the anchor keeps the bytes, and the
    /// removal log keeps the event.
    pub(crate) async fn plan_list_removes<S: Store>(
        &self,
        store: &S,
        batch: &mut Vec<Write>,
        lists: &mut [Chain<Vec<u8>>],
        id: Id,
        value: &T,
    ) -> Result<()> {
        for (i, chain) in lists.iter_mut().enumerate() {
            let key = Self::list_key(value, i, id);
            if let Some(writes) = chain.plan_remove(store, &key).await? {
                batch.extend(writes);
            }
        }
        Ok(())
    }

    /// Stream every living record in declared list `index`'s order, ascending.
    ///
    /// One read per segment, records inline — the same shape as
    /// [`all`](Collection::all), in a declared order instead of recency order.
    /// The generated `listed_by_<fields>` wrapper calls this with the
    /// declaration's index.
    pub fn listed<'a, S: Store>(
        self,
        store: &'a S,
        index: usize,
    ) -> impl Stream<Item = Result<(Id, T)>> + 'a
    where
        T: 'a,
    {
        self.listed_from(store, index, None)
    }

    /// [`listed`](Self::listed) entered at global offset `offset` — the pager's
    /// "jump to page k", one sparse-index descent rather than a walk.
    ///
    /// The stream still runs to the end of the list; a caller takes the page it
    /// wants. An offset at or past the end yields nothing.
    pub fn listed_at<'a, S: Store>(
        self,
        store: &'a S,
        index: usize,
        offset: u64,
    ) -> impl Stream<Item = Result<(Id, T)>> + 'a
    where
        T: 'a,
    {
        self.listed_from(store, index, Some(offset))
    }

    /// How many living records declared list `index` holds — the pager's
    /// "of M", one read cold (the index root carries the sum).
    ///
    /// # Errors
    /// [`Error::ListOutOfRange`] for an undeclared index, or a [`Store`]
    /// failure.
    pub async fn list_len<S: Store>(
        &self,
        store: &S,
        index: usize,
    ) -> Result<u64> {
        let pivot = self.load_pivot(store).await?;
        let chain = self.list_chain(&pivot, index)?;
        match chain.index() {
            Some(tree) => tree.total(store).await,
            None => Ok(0),
        }
    }

    /// Both list readers: walk the chain forward from `offset` (or from the
    /// head), yielding whole segments' worth of records.
    fn listed_from<'a, S: Store>(
        self,
        store: &'a S,
        index: usize,
        offset: Option<u64>,
    ) -> impl Stream<Item = Result<(Id, T)>> + 'a
    where
        T: 'a,
    {
        let handle = self;
        futures::stream::once(async move {
            let pivot = handle.load_pivot(store).await?;
            let chain = handle.list_chain(&pivot, index)?;
            let tenant = handle.tenant();
            // Where the walk starts, and how far into that first segment: an
            // offset descent when one was asked for, the head otherwise.
            let start = match (offset.filter(|at| *at > 0), chain.index()) {
                (Some(at), Some(tree)) => tree
                    .find_offset(store, at)
                    .await?
                    .map(|(slot, within)| (slot.seg, within)),
                // No index to descend, so no way to skip: an index-less chain
                // is the removal log's shape, which is never a declared list.
                (Some(_), None) => None,
                // Offset zero is the head by definition — no descent, and no
                // dependence on the index for the common whole-list read.
                (None, _) => Some((chain.head(), 0)),
            };
            Ok::<_, Error>(
                futures::stream::try_unfold(start, move |cursor| async move {
                    let Some((id, skip)) = cursor else {
                        return Ok::<_, Error>(None);
                    };
                    let seg = chain.segment(store, id).await?;
                    let mut page = Vec::with_capacity(seg.len());
                    // Ascending, and no reverse: a declared list reads in its
                    // own order, where recency reads backwards.
                    for (key, bytes) in
                        seg.entries().skip(usize::try_from(skip).unwrap_or(0))
                    {
                        let (_, value) =
                            decode_record::<T>(T::STRUCT_HASH, bytes)?;
                        page.push(Ok((key.rec.to_id(tenant), value)));
                    }
                    Ok(Some((
                        futures::stream::iter(page),
                        seg.next().map(|next| (next, 0)),
                    )))
                })
                .try_flatten(),
            )
        })
        .try_flatten()
    }
}
