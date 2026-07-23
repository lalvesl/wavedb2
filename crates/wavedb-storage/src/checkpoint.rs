//! Checkpointing — persisting the journal's projection so the log truncates.
//!
//! A checkpoint is one block run in `data.bin` holding the engine's
//! **metadata snapshot**: every type's directory slots and dictionary run,
//! plus the allocator's file length. The page bytes themselves are already
//! on disk (settle is inline with `apply`), so committing a checkpoint is:
//! sync the data file, write the snapshot run, sync, repoint the superblock
//! (the atomic commit — one block-0 rewrite), then truncate the journal.
//!
//! ## Crash windows
//!
//! - Before the superblock repoint: the old checkpoint (or none) still
//!   rules, and the journal still holds everything — recovery replays it.
//! - After the repoint, before the journal truncate: the checkpoint covers
//!   the **whole** journal at that moment, and replaying already-covered
//!   frames over checkpoint state converges (the last write per id wins
//!   through the same commit + settle path) — so replaying the stale log is
//!   harmless.
//!
//! Runs the durable checkpoint points at must not be reused until the next
//! checkpoint commits — the allocator's protected set (see
//! [`crate::alloc`]) defers those frees.
//!
//! A **corrupt** checkpoint run refuses to open (no silent fallback):
//! pre-release format policy — an unreadable `data.bin` is unsupported, not
//! migrated around.

use wavedb_core::wire::{WaveWire, from_wire_checked, to_wire_checked};

use crate::alloc::BlockAllocator;
use crate::block::{BLOCK_SIZE, BlockDescriptor, Run};
use crate::block_file::{BlockFile, RESERVED_BLOCKS};
use crate::dictionary::Dictionary;
use crate::directory::Directory;
use crate::error::{StorageError, StorageResult};
use crate::page_store::PageStore;
use crate::struct_storage::StructStorage;

/// Per-run prefix: `payload_len (u32)` — the same framing every engine
/// structure uses.
const LEN_PREFIX: usize = 4;

/// One type's snapshot: its directory's slots and its dictionary run.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
struct CheckpointEntry {
    struct_hash: u64,
    slots: Vec<BlockDescriptor>,
    dict: BlockDescriptor,
}

/// The whole snapshot a checkpoint run persists.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
struct CheckpointBody {
    entries: Vec<CheckpointEntry>,
    /// The allocator's file length (in blocks) at commit — including the
    /// checkpoint run itself.
    total_blocks: u64,
}

impl PageStore {
    /// Commit a checkpoint: after this returns, `data.bin` alone recovers
    /// the store and the journal is empty. Holds the journal lock for the
    /// duration (writers quiesce; reads proceed).
    ///
    /// # Errors
    /// A write/sync fault. The store stays consistent either way — until
    /// the superblock repoint lands, the previous recovery source rules.
    #[allow(clippy::significant_drop_tightening)]
    pub fn checkpoint(&self) -> StorageResult<()> {
        let mut journal = self.journal.lock();

        // The snapshot below reads the directories, so every committed
        // batch must be settled first (writers are quiesced by the journal
        // lock, so the queue stays drained through the commit).
        self.drain()?;

        // Snapshot every settled type's metadata + collect the used runs.
        let mut entries = Vec::new();
        let mut used = vec![Run::new(0, RESERVED_BLOCKS)];
        for slot in &self.types {
            let dir_guard = slot.directory().lock();
            let Some(dir) = dir_guard.as_ref() else {
                continue; // nothing ever settled for this type
            };
            let dict = slot.dictionary().lock();
            used.extend(
                dir.slots()
                    .iter()
                    .filter(|d| d.is_allocated())
                    .map(|d| d.run()),
            );
            if dict.descriptor().is_allocated() {
                used.push(dict.descriptor().run());
            }
            entries.push(CheckpointEntry {
                struct_hash: slot.struct_hash(),
                slots: dir.slots().to_vec(),
                dict: dict.descriptor(),
            });
        }

        let mut alloc = self.alloc.lock();
        let old = self.file.checkpoint();

        // Write the snapshot run, then make *everything* durable before the
        // pointer moves — the superblock must never name a half-written run.
        let mut body = CheckpointBody {
            entries,
            total_blocks: 0,
        };
        let payload_len = LEN_PREFIX + checked_len(&body);
        let run = alloc.alloc(payload_len.div_ceil(BLOCK_SIZE) as u64);
        body.total_blocks = alloc.total_blocks();
        let bytes = encode(&body);
        self.file.write_run(run, &bytes)?;
        self.file.sync()?;

        // The commit point: one durable block-0 rewrite retires `old`.
        let desc = BlockDescriptor::from_run_used(run, bytes.len() as u64);
        self.file.set_checkpoint(desc)?;

        // The new checkpoint's runs are now the protected set; the old
        // run — and every free deferred under the old protection — releases.
        used.push(run);
        alloc.set_protected(&used);
        if old.is_allocated() {
            alloc.free(old.run());
        }

        journal.truncate()
    }
}

/// Restore engine state from the committed checkpoint, if one exists:
/// directories and dictionaries load into the (already reset) slots, and
/// the returned allocator frees exactly the gaps between the persisted
/// runs. Caches stay empty — reads fall through to the pages.
///
/// # Errors
/// [`StorageError::Corrupt`] on an unreadable checkpoint (refuse, don't
/// fall back); [`StorageError::UnregisteredStructHash`] when the checkpoint
/// names a type this open's registry does not list.
pub fn restore(
    file: &BlockFile,
    types: &[&'static StructStorage],
) -> StorageResult<Option<BlockAllocator>> {
    let desc = file.checkpoint();
    if !desc.is_allocated() {
        return Ok(None);
    }
    let body = decode(&file.read_run(desc.run())?)?;

    let mut used = vec![Run::new(0, RESERVED_BLOCKS), desc.run()];
    for entry in &body.entries {
        let slot = types
            .binary_search_by_key(&entry.struct_hash, |s| s.struct_hash())
            .map(|i| types[i])
            .map_err(|_| {
                StorageError::UnregisteredStructHash(entry.struct_hash)
            })?;
        used.extend(
            entry
                .slots
                .iter()
                .filter(|d| d.is_allocated())
                .map(|d| d.run()),
        );
        *slot.directory().lock() =
            Some(Directory::from_slots(entry.slots.clone(), file.seed()));
        if entry.dict.is_allocated() {
            used.push(entry.dict.run());
            let dict =
                Dictionary::from_bytes(&file.read_run(entry.dict.run())?)?;
            slot.dictionary().lock().load(dict, entry.dict);
        }
    }

    let mut alloc = BlockAllocator::from_layout(body.total_blocks, &used);
    alloc.set_protected(&used);
    Ok(Some(alloc))
}

/// `[len (u32 LE)][to_wire_checked(body)]`.
fn encode(body: &CheckpointBody) -> Vec<u8> {
    let payload = to_wire_checked(body);
    let mut out = Vec::with_capacity(LEN_PREFIX + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Byte length [`encode`] will produce for `body`, minus the prefix.
fn checked_len(body: &CheckpointBody) -> usize {
    to_wire_checked(body).len()
}

/// Parse a checkpoint back from its (zero-padded) block run.
fn decode(buf: &[u8]) -> StorageResult<CheckpointBody> {
    let len_bytes: [u8; LEN_PREFIX] = buf
        .get(..LEN_PREFIX)
        .and_then(|s| s.try_into().ok())
        .ok_or(StorageError::Corrupt("checkpoint length truncated"))?;
    let len = u32::from_le_bytes(len_bytes) as usize;
    let end = LEN_PREFIX
        .checked_add(len)
        .filter(|&end| end <= buf.len())
        .ok_or(StorageError::Corrupt("checkpoint length out of range"))?;
    from_wire_checked(&buf[LEN_PREFIX..end])
        .map_err(|_| StorageError::Corrupt("checkpoint body"))
}
