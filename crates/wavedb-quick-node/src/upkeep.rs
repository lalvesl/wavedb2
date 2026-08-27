//! The node's background upkeep: what it asks the disk actor for, on a timer.
//!
//! Split from `lib.rs` for the file budget, and it reads better apart anyway:
//! everything here is *policy* — when to suggest work — while the actor
//! decides when to do it.
//!
//! Hints, not calls. Once the actor owns the engine (RFC 0064) this loop can
//! only suggest, and the actor weighs each suggestion against the reads it is
//! also serving (`shard::priority`). A hint dropped because the queue is full
//! is a hint the actor has already been given.

use crate::{Maintenance, ServerError, shard};

/// The background maintenance loop: periodically settle the pending queue,
/// checkpoint once the journal crosses the threshold, and evict settled
/// cache entries down to budget. An engine fault stops maintenance (acked
/// writes stay safe in the journal); serving continues.
pub async fn maintain(disk: shard::DiskHandle, policy: Maintenance) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // Hints, not calls: the actor owns the engine, and it decides *when*
        // to act on them against the reads it is also serving (`shard::
        // priority`). A hint dropped because the queue is full is a hint the
        // actor has already been given.
        let Ok(stats) = engine_stats(&disk).await else {
            return; // the actor is gone; serving is over too
        };
        disk.hint(shard::Maintenance::Settle);
        if stats.journal_bytes > policy.checkpoint_after_bytes {
            disk.hint(shard::Maintenance::Checkpoint);
        }
        disk.hint(shard::Maintenance::Evict {
            budget_bytes: policy.cache_budget_bytes,
        });
        // A checkpoint leaves its `Commit` frame unsynced for the next write to
        // carry, for free, and its retired journal on disk until the next
        // checkpoint disposes of it (RFC 0047). Maintenance has nothing to do
        // about either: forcing the barrier here would spend the IOp the
        // deferral exists to save, to reclaim disk — the abundant resource.
        // Keep a window large enough for the next checkpoint to land in a
        // hole rather than at the tail; a pass that finds nothing is free.
        if stats.largest_free_extent < policy.defrag_below_blocks {
            disk.hint(shard::Maintenance::Defragment {
                budget_blocks: policy.defrag_budget_blocks,
            });
        }
    }
}

/// Ask the actor what the engine holds. `Err` means the actor is gone.
async fn engine_stats(
    disk: &shard::DiskHandle,
) -> std::result::Result<shard::EngineStats, ()> {
    let (answer, wait) = tokio::sync::oneshot::channel();
    disk.send(shard::DiskRequest::Stats { answer })
        .await
        .map_err(|_| ())?;
    wait.await.map_err(|_| ())?.map_err(|_| ())
}

/// The engine's owner is gone — nothing can be asked of it again.
pub const fn stopped() -> ServerError {
    ServerError::Storage(wavedb_storage::StorageError::Corrupt(
        "disk actor stopped before the node did",
    ))
}
