//! Journal commit + recovery — the journal's crc framing is the engine's
//! **only** atomicity mechanism (user-directed design, 2026-07-07).
//!
//! ## Commit ( [`PageStore::commit_journal`] )
//!
//! ```text
//! 1. rotate: create journal_<ts+1>.log, swap under the append lock (µs —
//!    writers redirect, no settle work under the lock)
//! 2. drain: settle everything queued into pages — ONE window write
//!    (RFC 0041), then fsync `data.bin`
//! 3. append ONE `Commit { done: old_ts, slots: EVERY type's descriptor
//!    vector, dicts }` frame to the NEW journal + fsync. The addressing
//!    state travels *in* the frame — `data.bin` holds no directory
//!    structure — so a checkpoint's metadata is one journal IOp
//!    (RFC 0043)
//! 4. delete the old journal
//! 5. re-protect: the new commit's runs become the allocator's protected
//!    set, releasing frees deferred under the previous one
//! ```
//!
//! ## Recovery ( [`restore`] )
//!
//! Scan `journal_*.log` sorted by timestamp; the **newest decodable
//! `Commit`** is the base: its `slots` *are* the directories (and its
//! `dicts` the dictionary runs), the allocator derives from those page and
//! dictionary runs, journals it covers (`ts <= journal_ts`) are skipped (and
//! cleaned up), and every `Batch` in the remaining journals replays through
//! the normal commit + settle path (re-settling is idempotent). A torn
//! `Commit` frame is invisible — the retired journal is still on disk and
//! rules. No `Commit` anywhere ⇒ first-generation full rebuild.

use std::path::Path;

use wavedb_core::Write;

use crate::alloc::BlockAllocator;
use crate::block::{BlockDescriptor, Run};
use crate::block_file::{BlockFile, RESERVED_BLOCKS};
use crate::dictionary::Dictionary;
use crate::directory::Directory;
use crate::error::{StorageError, StorageResult};
use crate::journal::{self, CommitFrame, Journal, JournalFrame};
use crate::page_store::PageStore;
use crate::struct_storage::StructStorage;

impl PageStore {
    /// Retire the current journal: rotate, settle everything it covers, then
    /// append the atomic `Commit` frame — carrying every type's descriptor
    /// vector and the retired journal's DONE marker — to the new journal.
    /// After this returns the old journal file is gone and recovery roots in
    /// the new one.
    ///
    /// # Errors
    /// A write/sync fault. Until the `Commit` frame lands, the old journal
    /// (and the previous commit) still rule — nothing acked is at risk.
    // The alloc/dir guards span the snapshot loop by design: the frame must
    // name exactly the state that was just synced, with no write landing in
    // between.
    #[allow(clippy::significant_drop_tightening)]
    pub fn commit_journal(&self) -> StorageResult<()> {
        // 1. Rotate under the append lock — writers redirect immediately.
        let old = {
            let mut journal = self.journal.lock();
            let fresh = Journal::create(
                &self.data_dir,
                journal::next_ts(journal.ts()),
            )?;
            std::mem::replace(&mut *journal, fresh)
        };

        // 2. Everything the old journal holds settles into pages — one
        // planned window, one positioned write (RFC 0041). Writes landing in
        // the new journal may settle too: harmless, their journal survives
        // and re-settling converges.
        self.drain()?;

        // Snapshot: the addressing state the frame will carry, and every run
        // it references (the next protected set).
        let mut alloc = self.alloc.lock();
        let mut slots = Vec::with_capacity(self.types.len());
        let mut dicts = Vec::with_capacity(self.types.len());
        let mut used = vec![Run::new(0, RESERVED_BLOCKS)];
        for slot in &self.types {
            let dir_guard = slot.directory().lock();
            let descriptors: Vec<u64> =
                dir_guard.as_ref().map_or_else(Vec::new, |dir| {
                    dir.slots().iter().map(|d| d.raw()).collect()
                });
            used.extend(
                descriptors
                    .iter()
                    .map(|&raw| BlockDescriptor::from_raw(raw))
                    .filter(|d| d.is_allocated())
                    .map(BlockDescriptor::run),
            );
            let dict = slot.dictionary().lock();
            if dict.descriptor().is_allocated() {
                used.push(dict.descriptor().run());
            }
            slots.push((slot.struct_hash(), descriptors));
            dicts.push((slot.struct_hash(), dict.descriptor().raw()));
        }
        // The pages must be durable before the frame addressing them.
        self.file.sync()?;

        // 3. The atomic commit: the whole addressing state and the DONE
        // marker in ONE crc-framed append (fsync inside).
        self.journal
            .lock()
            .append(&JournalFrame::Commit(CommitFrame {
                journal_ts: old.ts(),
                slots,
                dicts,
            }))?;

        // 4. The retired journal's history is now redundant.
        old.delete()?;

        // 5. Protection rolls forward; frees deferred under the previous
        // commit release.
        alloc.set_protected(&used);
        Ok(())
    }
}

/// What [`restore`] recovered: the allocator and the batches still to
/// replay (in order), plus the journal to keep appending to.
pub struct Recovered {
    pub alloc: BlockAllocator,
    pub journal: Journal,
    pub replay: Vec<Vec<Write>>,
}

/// Recover engine state from the journals under `dir` (see module docs).
/// The slots must already be reset; on return the recovered directories and
/// dictionaries are loaded into them, caches empty.
///
/// # Errors
/// [`StorageError::Corrupt`] on a lost recovery root (`data.bin` with no
/// journal) or an unreadable chain; [`StorageError::UnregisteredStructHash`]
/// when a commit names a type this open's registry does not list.
pub fn restore(
    dir: &Path,
    data_bin_existed: bool,
    file: &BlockFile,
    types: &[&'static StructStorage],
) -> StorageResult<Recovered> {
    let found = journal::scan(dir)?;
    journal::require_journal_for(data_bin_existed, &found)?;

    // Fresh database: first journal, empty everything.
    if found.is_empty() {
        let journal = Journal::create(dir, journal::next_ts(0))?;
        return Ok(Recovered {
            alloc: fresh_alloc(file)?,
            journal,
            replay: Vec::new(),
        });
    }

    // Replay every journal present; remember the newest decodable Commit.
    let mut journals = Vec::with_capacity(found.len());
    let mut newest_commit: Option<CommitFrame> = None;
    for (ts, path) in &found {
        let mut journal = Journal::open(path, *ts)?;
        let frames = journal.replay()?;
        for frame in &frames {
            if let JournalFrame::Commit(c) = frame {
                newest_commit = Some(c.clone());
            }
        }
        journals.push((*ts, journal, frames));
    }

    let (alloc, covered_ts) = if let Some(commit) = &newest_commit {
        (load_commit(file, types, commit)?, commit.journal_ts)
    } else {
        // First generation (never rotated): rebuild pages from scratch.
        (fresh_alloc(file)?, 0)
    };

    // Batches from journals the commit does not cover, oldest first. The
    // covered ones only linger after a crash between commit and delete —
    // clean them up now.
    let mut replay = Vec::new();
    let mut current = None;
    for (ts, journal, frames) in journals {
        if newest_commit.is_some() && ts <= covered_ts {
            journal.delete()?;
            continue;
        }
        for frame in frames {
            if let JournalFrame::Batch(batch) = frame {
                replay.push(batch);
            }
        }
        current = Some(journal); // the newest survives the loop
    }
    let journal = match current {
        Some(j) => j,
        // Every journal was covered and deleted — start a fresh one.
        None => Journal::create(dir, journal::next_ts(covered_ts))?,
    };
    Ok(Recovered {
        alloc,
        journal,
        replay,
    })
}

/// Install a `Commit`'s directories into the slots and derive the allocator
/// from everything the commit references.
///
/// The frame *is* the addressing state — no chain to walk, no extra read.
fn load_commit(
    file: &BlockFile,
    types: &[&'static StructStorage],
    commit: &CommitFrame,
) -> StorageResult<BlockAllocator> {
    let mut used = vec![Run::new(0, RESERVED_BLOCKS)];
    for (hash, addresses) in &commit.slots {
        let slot = slot_of(types, *hash)?;
        if addresses.is_empty() {
            continue; // the type never settled anything
        }
        let descriptors: Vec<BlockDescriptor> = addresses
            .iter()
            .map(|&a| BlockDescriptor::from_raw(a))
            .collect();
        used.extend(
            descriptors
                .iter()
                .filter(|d| d.is_allocated())
                .map(|d| d.run()),
        );
        *slot.directory().lock() =
            Some(Directory::from_slots(descriptors, file.seed()));
    }
    for &(hash, desc_raw) in &commit.dicts {
        let slot = slot_of(types, hash)?;
        let desc = BlockDescriptor::from_raw(desc_raw);
        if !desc.is_allocated() {
            continue;
        }
        used.push(desc.run());
        let dict = Dictionary::from_bytes(&file.read_run(desc.run())?)?;
        slot.dictionary().lock().load(dict, desc);
    }
    let mut alloc = BlockAllocator::from_layout(file.len_blocks()?, &used);
    alloc.set_protected(&used);
    Ok(alloc)
}

/// The registered slot for `hash` — a commit naming an unlisted type
/// refuses, like every other allowlist edge.
fn slot_of(
    types: &[&'static StructStorage],
    hash: u64,
) -> StorageResult<&'static StructStorage> {
    types
        .binary_search_by_key(&hash, |s| s.struct_hash())
        .map(|i| types[i])
        .map_err(|_| StorageError::UnregisteredStructHash(hash))
}

/// An allocator over a page-less `data.bin` (superblock only).
fn fresh_alloc(file: &BlockFile) -> StorageResult<BlockAllocator> {
    file.truncate_to_blocks(RESERVED_BLOCKS)?;
    let mut alloc = BlockAllocator::new();
    alloc.alloc(RESERVED_BLOCKS);
    Ok(alloc)
}
