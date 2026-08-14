//! Deferred retirement — the second half of a checkpoint, held until its
//! `Commit` frame is durable ([RFC 0046]).
//!
//! Since the frame carries only *pointers* into an addressing log `data.bin`
//! already made durable, it needs no barrier of its own: it is written
//! unsynced and the next ordinary `Batch` append's `fsync` carries it, since
//! `fsync` flushes the file rather than one write. A checkpoint therefore
//! costs **one** barrier — the `data.bin` sync — instead of two.
//!
//! What has to wait for that durability is everything the frame *authorises*:
//!
//! - **deleting the retired journal.** If the frame is torn, recovery falls
//!   back to the previous `Commit`, and that journal's batches are the only
//!   record of what happened since.
//! - **rolling the allocator's protected set forward.** Rolling it releases
//!   the frees deferred under the *previous* commit — the runs a fallback
//!   recovery would still read.
//!
//! Both are held in a [`Retiring`] until [`PageStore::finish_retirement`]
//! observes the frame durable. There is at most one at a time:
//! [`commit_journal`](PageStore::commit_journal) forces any pending one before
//! it rotates, so the pending frame always lives in the *current* journal.
//!
//! [RFC 0046]: ../../../rfcs/0046-directory-deltas-in-the-window.md

use crate::block::Run;
use crate::error::StorageResult;
use crate::journal::Journal;
use crate::page_store::PageStore;

/// A checkpoint whose `Commit` frame is written but not yet durable.
#[derive(Debug)]
pub struct Retiring {
    /// The retired journal, deleted once the frame that retires it is durable.
    pub journal: Journal,
    /// The protected set that frame implies — the next durable checkpoint's
    /// runs.
    pub used: Vec<Run>,
}

impl PageStore {
    /// Complete a pending retirement whose frame the caller just made durable
    /// (an ordinary `Batch` append). A no-op when nothing is pending.
    ///
    /// Costs one `unlink`, no barrier — the accounting that makes deferral
    /// worth it.
    ///
    /// # Errors
    /// [`StorageError::Io`](crate::StorageError::Io) if the unlink fails —
    /// a broken data directory, which the next write would hit anyway. The
    /// protection roll happens **first** precisely so that failure cannot
    /// strand the deferred frees; the leftover file is then cleaned up by
    /// recovery, which sees it covered by the durable frame.
    pub(crate) fn finish_retirement(&self) -> StorageResult<()> {
        // Take on its own line, deliberately: `commit_journal` publishes the
        // retirement while holding the allocator, so this path must never
        // hold `retiring` *across* `alloc.lock()` — folding these two
        // statements into one `if let` would nest them the other way round
        // and deadlock a checkpoint against a concurrent write.
        let taken = self.retiring.lock().take();
        let Some(retiring) = taken else {
            return Ok(());
        };
        // The frame's durability is what authorises both steps, so their
        // order is free — and the infallible one goes first.
        self.alloc.lock().set_protected(&retiring.used);
        retiring.journal.delete()
    }

    /// Make a pending checkpoint's frame durable **now** and complete it.
    ///
    /// A no-op when nothing is pending — the ordinary case on a busy node,
    /// where a `Batch` append has already carried the frame for free. Call it
    /// when no such append is coming: an idle maintenance tick, a graceful
    /// shutdown, or before the next checkpoint rotates.
    ///
    /// # Errors
    /// A sync or unlink fault. Nothing acked is at risk: until this succeeds
    /// the retired journal is still on disk and still rules.
    pub fn force_retirement(&self) -> StorageResult<()> {
        if self.retiring.lock().is_none() {
            return Ok(());
        }
        self.journal.lock().sync()?;
        self.finish_retirement()
    }

    /// Whether a checkpoint is still waiting for its frame to become durable.
    #[must_use]
    pub fn is_retiring(&self) -> bool {
        self.retiring.lock().is_some()
    }
}
