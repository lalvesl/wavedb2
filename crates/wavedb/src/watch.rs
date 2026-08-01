//! Live watch (M7 W5) — typed subscription streams that keep the local
//! cache warm.
//!
//! [`Db::watch_unique`] / [`Db::watch_collection`] register with the
//! process's connection manager ([`wavedb_net::manager`]): every watch of
//! one identity shares ONE WebSocket connection (`Hello` once, each topic
//! subscribed once, events fanned out) — or, on a handle configured with
//! [`watch_via_polling`](Db::watch_via_polling), rides "anything new?"
//! POST polls instead. Either way the call returns only once the
//! subscription is **live** (acked / registered node-side), so a mutation
//! committed after a watch exists cannot be missed, and no pumping falls
//! on the watcher — the manager pushes each event into the watch's channel
//! as it arrives.
//!
//! Every event **mirrors into the M6 cache before it is yielded**
//! (`client_cache`'s `mirror_*` — best-effort, absent when the
//! handle has no cache): a watcher is the live half of sync, keeping local
//! reads warm. Events decode to the watched type; the record's
//! authoritative id rides along ([`WatchEvent`]).
//!
//! A watch needs an authenticated handle
//! ([`with_access_token`](Db::with_access_token)) — the node refuses
//! anonymous subscriptions (tenant isolation is bound to the verified
//! token), so the refusal here is typed and immediate instead of a closed
//! socket.

use std::marker::PhantomData;

use futures::StreamExt;
use futures::channel::mpsc;
use wavedb_core::wire::from_wire;
use wavedb_core::{Id, NonUniqueStruct, PivotHandle, UniqueStruct};
use wavedb_net::manager::{self, WatchGuard};
use wavedb_net::ws::{EventKind, RecordEvent, Topic};

use crate::client_cache::{
    mirror_record, mirror_record_meta, mirror_remove, mirror_unique,
    mirror_unique_meta,
};
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
    events: mpsc::UnboundedReceiver<RecordEvent>,
    _guard: WatchGuard,
    db: Db,
    _record: PhantomData<T>,
}

/// A live watch on one collection — see [`Db::watch_collection`].
#[derive(Debug)]
pub struct CollectionWatch<T> {
    events: mpsc::UnboundedReceiver<RecordEvent>,
    _guard: WatchGuard,
    db: Db,
    _record: PhantomData<T>,
}

impl Db {
    /// Watch `T`'s Unique anchor under this handle's tenant: every
    /// acknowledged save arrives as a [`WatchEvent::Saved`], mirrored into
    /// the local cache first.
    ///
    /// # Errors
    /// [`Error::Unauthorized`] on a token-less handle, [`Error::Transport`]
    /// on a connect/handshake/poll fault or a refused identity.
    pub async fn watch_unique<T: UniqueStruct>(
        &self,
    ) -> Result<UniqueWatch<T>> {
        let topic = Topic {
            struct_hash: T::STRUCT_HASH,
            pivot: None,
        };
        let (events, guard) = self.subscribed(topic).await?;
        Ok(UniqueWatch {
            events,
            _guard: guard,
            db: self.clone(),
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
        let (events, guard) = self.subscribed(topic).await?;
        Ok(CollectionWatch {
            events,
            _guard: guard,
            db: self.clone(),
            _record: PhantomData,
        })
    }

    /// Register the watch with the connection manager — live on return.
    async fn subscribed(
        &self,
        topic: Topic,
    ) -> Result<(mpsc::UnboundedReceiver<RecordEvent>, WatchGuard)> {
        if !self.has_token() {
            return Err(Error::unauthorized(
                "a watch needs an authenticated session — the node refuses \
                 anonymous subscriptions",
            ));
        }
        manager::watch(self.addr(), self.auth(), self.watch_mode(), topic)
            .await
            .map_err(Error::Transport)
    }
}

impl<T: UniqueStruct> UniqueWatch<T> {
    /// The next mutation; `None` = the watch ended **permanently** — a fatal
    /// identity refusal, or the last handle dropped. A transient socket drop no
    /// longer ends the stream: the connection manager reconnects and catches up
    /// (W6), and a poll watch rides outages silently.
    ///
    /// # Errors
    /// [`Error::Core`] on a body that does not decode as `T`.
    pub async fn next(&mut self) -> Result<Option<WatchEvent<T>>> {
        let Some(event) = self.events.next().await else {
            return Ok(None);
        };
        let typed = decode::<T>(&event)?;
        if let WatchEvent::Saved(_, value) = &typed {
            // The event's metadata is the node's chain data — adopt it
            // verbatim (a meta-less event falls back to authoring locally).
            match event.meta {
                Some(meta) => {
                    mirror_unique_meta(&self.db, meta, value).await;
                }
                None => mirror_unique(&self.db, value).await,
            }
        }
        Ok(Some(typed))
    }
}

impl<T: NonUniqueStruct> CollectionWatch<T> {
    /// The next mutation; `None` = the watch ended **permanently** — a fatal
    /// identity refusal, or the last handle dropped. A transient socket drop no
    /// longer ends the stream: the connection manager reconnects and catches up
    /// (W6), and a poll watch rides outages silently.
    ///
    /// # Errors
    /// [`Error::Core`] on a body that does not decode as `T`.
    pub async fn next(&mut self) -> Result<Option<WatchEvent<T>>> {
        let Some(event) = self.events.next().await else {
            return Ok(None);
        };
        let Some(pivot) = event.topic.pivot else {
            unreachable!("a collection watch only subscribes with a pivot");
        };
        let typed = decode::<T>(&event)?;
        match &typed {
            WatchEvent::Saved(id, value) => match event.meta {
                Some(meta) => {
                    mirror_record_meta(&self.db, pivot, *id, meta, value).await;
                }
                None => mirror_record(&self.db, pivot, *id, value).await,
            },
            WatchEvent::Removed(id) => {
                mirror_remove::<T>(&self.db, pivot, *id).await;
            }
        }
        Ok(Some(typed))
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
        // The removal instant is cursor bookkeeping (the manager's poll
        // loop consumed it); the typed surface names the record alone.
        EventKind::Removed(_) => Ok(WatchEvent::Removed(event.id)),
    }
}
