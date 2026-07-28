//! Live watch (M7 W5) — typed subscription streams that keep the local
//! cache warm.
//!
//! [`Db::watch_unique`] / [`Db::watch_collection`] open one WebSocket
//! session each ([`WsSession`]; multiplexing many watches over one
//! connection is a later refinement), present the handle's identity once,
//! and subscribe to exactly one topic — a Unique anchor or one collection
//! `Pivot`, the same granularity a screen shows. The call returns only
//! after the node acked the subscription, so a mutation committed after a
//! watch exists **cannot be missed**.
//!
//! Every event **mirrors into the M6 cache before it is yielded**
//! ([`crate::client_cache`]'s `mirror_*` — best-effort, absent when the
//! handle has no cache): a watcher is the live half of sync, keeping local
//! reads warm without polling. Events decode to the watched type; the
//! record's authoritative id rides along ([`WatchEvent`]).
//!
//! A watch needs an authenticated handle
//! ([`with_access_token`](Db::with_access_token)) — the node refuses
//! anonymous subscriptions, so the refusal here is typed and immediate
//! instead of a closed socket.

use std::marker::PhantomData;

use wavedb_core::wire::from_wire;
use wavedb_core::{Id, NonUniqueStruct, PivotHandle, UniqueStruct};
use wavedb_net::WsSession;
use wavedb_net::ws::{EventKind, RecordEvent, Topic};

use crate::client_cache::{mirror_record, mirror_remove, mirror_unique};
use crate::db::Db;
use crate::error::{Error, Result};

/// One typed mutation a watcher observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent<T> {
    /// Inserted, updated, or Unique-saved — carries the record's
    /// authoritative id (the anchor for Unique) and its new state.
    Saved(Id, T),
    /// Removed from the living set.
    Removed(Id),
}

/// A live watch on a Unique anchor — see [`Db::watch_unique`].
#[derive(Debug)]
pub struct UniqueWatch<T> {
    session: WsSession,
    db: Db,
    topic: Topic,
    _record: PhantomData<T>,
}

/// A live watch on one collection — see [`Db::watch_collection`].
#[derive(Debug)]
pub struct CollectionWatch<T> {
    session: WsSession,
    db: Db,
    topic: Topic,
    _record: PhantomData<T>,
}

impl Db {
    /// Watch `T`'s Unique anchor under this handle's tenant: every
    /// acknowledged save arrives as a [`WatchEvent::Saved`], mirrored into
    /// the local cache first.
    ///
    /// # Errors
    /// [`Error::Unauthorized`] on a token-less handle, [`Error::Transport`]
    /// on a connect/handshake fault or a refused identity.
    pub async fn watch_unique<T: UniqueStruct>(
        &self,
    ) -> Result<UniqueWatch<T>> {
        let topic = Topic {
            struct_hash: T::STRUCT_HASH,
            pivot: None,
        };
        let session = self.subscribed(topic).await?;
        Ok(UniqueWatch {
            session,
            db: self.clone(),
            topic,
            _record: PhantomData,
        })
    }

    /// Watch the `T` collection behind `pivot` under this handle's tenant:
    /// inserts and updates arrive as [`WatchEvent::Saved`] (node-minted
    /// ids), removals as [`WatchEvent::Removed`] — each mirrored into the
    /// local cache first, so the walk stays warm without polling.
    ///
    /// # Errors
    /// As [`watch_unique`](Self::watch_unique).
    pub async fn watch_collection<T>(
        &self,
        pivot: T::PivotId,
    ) -> Result<CollectionWatch<T>>
    where
        T: NonUniqueStruct,
        T::PivotId: PivotHandle,
    {
        let topic = Topic {
            struct_hash: T::STRUCT_HASH,
            pivot: Some(pivot.local_id()),
        };
        let session = self.subscribed(topic).await?;
        Ok(CollectionWatch {
            session,
            db: self.clone(),
            topic,
            _record: PhantomData,
        })
    }

    /// Open the watch session: identity, then the acked subscription.
    async fn subscribed(&self, topic: Topic) -> Result<WsSession> {
        if !self.has_token() {
            return Err(Error::unauthorized(
                "a watch needs an authenticated session — the node refuses \
                 anonymous subscriptions",
            ));
        }
        let mut session = WsSession::open(self.addr(), self.auth())
            .await
            .map_err(Error::Transport)?;
        session.subscribe(topic).await.map_err(Error::Transport)?;
        Ok(session)
    }
}

impl<T: UniqueStruct> UniqueWatch<T> {
    /// The next mutation; `None` = the node closed the connection.
    ///
    /// # Errors
    /// [`Error::Transport`] on a socket fault, [`Error::Core`] on a body
    /// that does not decode as `T`.
    pub async fn next(&mut self) -> Result<Option<WatchEvent<T>>> {
        let Some(event) = next_for(&mut self.session, self.topic).await? else {
            return Ok(None);
        };
        let typed = decode::<T>(&event)?;
        if let WatchEvent::Saved(_, value) = &typed {
            mirror_unique(&self.db, value).await;
        }
        Ok(Some(typed))
    }
}

impl<T: NonUniqueStruct> CollectionWatch<T> {
    /// The next mutation; `None` = the node closed the connection.
    ///
    /// # Errors
    /// As [`UniqueWatch::next`].
    pub async fn next(&mut self) -> Result<Option<WatchEvent<T>>> {
        let Some(event) = next_for(&mut self.session, self.topic).await? else {
            return Ok(None);
        };
        let Some(pivot) = event.topic.pivot else {
            unreachable!("a collection watch only subscribes with a pivot");
        };
        let typed = decode::<T>(&event)?;
        match &typed {
            WatchEvent::Saved(id, value) => {
                mirror_record(&self.db, pivot, *id, value).await;
            }
            WatchEvent::Removed(id) => {
                mirror_remove::<T>(&self.db, pivot, *id).await;
            }
        }
        Ok(Some(typed))
    }
}

/// Pump the session until an event for `topic` arrives (the session holds
/// exactly one subscription, so a foreign topic is skipped defensively).
async fn next_for(
    session: &mut WsSession,
    topic: Topic,
) -> Result<Option<RecordEvent>> {
    loop {
        match session.next_event().await.map_err(Error::Transport)? {
            None => return Ok(None),
            Some(event) if event.topic == topic => return Ok(Some(event)),
            Some(_) => {}
        }
    }
}

/// Decode one wire event into the watched type.
fn decode<T: wavedb_core::WaveDbStruct>(
    event: &RecordEvent,
) -> Result<WatchEvent<T>> {
    match event.kind {
        EventKind::Saved => {
            let value = from_wire::<T>(&event.body)
                .map_err(wavedb_core::Error::from)
                .map_err(Error::Core)?;
            Ok(WatchEvent::Saved(event.id, value))
        }
        EventKind::Removed => Ok(WatchEvent::Removed(event.id)),
    }
}
