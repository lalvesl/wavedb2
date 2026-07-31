//! [`Collection`]'s recency half — the per-collection modification and
//! removal logs behind reconnect catch-up.
//!
//! Two system B+trees ride every collection, both keyed
//! `[instant BE (8 B)][anchor LocalId]` (a [`SecKey`], so the ordinary
//! secondary-tree machinery serves them):
//!
//! - **recency**: exactly one entry per **living** record, keyed by the
//!   instant its live version was authored. An insert adds it, a save
//!   re-keys it, a remove deletes it — so a tail scan from a cursor is
//!   precisely "every record changed since", each one once.
//! - **dead**: one entry per removed record, keyed by the instant of the
//!   removal — the removal log the same tail scan pairs with.
//!
//! Both are maintained inside the ops' single atomic batch. Their maxima
//! form the collection's **instant floor**: every instant minted for the
//! collection lands strictly above it (see [`crate::mint`]), so a rewound
//! clock can never author below a cursor a client already advanced past.

use crate::collection::Collection;
use crate::error::Result;
use crate::id::Id;
use crate::index::{BpTree, SecKey};
use crate::local_id::LocalId;
use crate::metadata::{Metadata, Succession};
use crate::overlay::Overlay;
use crate::store::{Store, Write};
use crate::traits::NonUniqueStruct;

impl<T: NonUniqueStruct> Collection<T> {
    /// The `[instant BE][anchor]` key the recency and dead trees use:
    /// instant-major (the logs are time-ordered), the anchor breaking ties
    /// and doubling as the leaf payload.
    pub(crate) fn instant_key(instant: u64, rec: LocalId) -> SecKey {
        SecKey {
            field: instant.to_be_bytes().to_vec(),
            rec,
        }
    }

    /// The instant a recency/dead key encodes.
    fn key_instant(key: &SecKey) -> u64 {
        // These trees are engine-written only; a non-8-byte field would
        // mean a corrupt node, which decode surfaces long before this.
        key.field
            .as_slice()
            .try_into()
            .map_or(0, u64::from_be_bytes)
    }

    /// The collection's instant floor: the greatest instant its recency
    /// and dead trees record (`0` when both are empty). Every instant
    /// minted for this collection must land strictly above it.
    pub(crate) async fn instant_floor<S: Store>(
        &self,
        store: &S,
        pivot: &T::Pivot,
    ) -> Result<u64> {
        use crate::index::Pivot as _;
        let recency = self.sec_tree(pivot.recency()).max_key(store).await?;
        let dead = self.sec_tree(pivot.dead()).max_key(store).await?;
        Ok(recency
            .iter()
            .chain(dead.iter())
            .map(Self::key_instant)
            .max()
            .unwrap_or(0))
    }

    /// Move the record's recency entry to the new live version's instant —
    /// but only when the old entry existed: a record outside the living set
    /// (a dead record saved by address, a chainless first version) must not
    /// enter the modification log through a save.
    pub(crate) async fn plan_recency_rekey<S: Store>(
        &self,
        view: &mut Overlay<'_, S>,
        batch: &mut Vec<Write>,
        recency: &mut BpTree<SecKey>,
        old_key: SecKey,
        live_meta: &Metadata,
        id: Id,
    ) -> Result<()> {
        let Some(writes) = recency.plan_remove(&*view, old_key).await? else {
            return Ok(());
        };
        view.stage(&writes);
        batch.extend(writes);
        let Succession::CreatedAt(instant) = live_meta.succession else {
            return Err(crate::Error::ChainCorrupt(id));
        };
        let writes = recency
            .plan_insert(
                &*view,
                Self::instant_key(instant, LocalId::from_id(id)),
            )
            .await?;
        view.stage(&writes);
        batch.extend(writes);
        Ok(())
    }
}
