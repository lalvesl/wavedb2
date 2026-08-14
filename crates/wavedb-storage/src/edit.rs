//! The addressing delta that rides every settle window ([RFC 0046]).
//!
//! A round of [`crate::checkpoint`] already writes one contiguous window and
//! already knows, by the time it has placed its pages, exactly which buckets
//! changed address. This module turns that into an [`EditChunk`] appended to
//! the same window: **the metadata costs no IOp of its own**, and the `Commit`
//! frame shrinks to a snapshot address plus the list of chunks written since.
//!
//! ## Two passes, one shape
//!
//! The chunk's byte length has to be known before the window is carved, but
//! its contents are only known after the runs are handed out. Both passes call
//! [`chunk_of`] with the same `full` flag and the same plans, so they build the
//! *same shape* — same slot count, same `changed` length, same `Option` tags —
//! and therefore encode to the same length. The first pass reserves; the second
//! fills in the addresses.
//!
//! ## Compaction
//!
//! [`MetaLog`] tracks the log and decides when a round should emit a **full**
//! chunk instead of a delta: one naming every bucket of every type, built from
//! the directories already resident in RAM. It supersedes the previous snapshot
//! and every chunk after it, which are freed through the allocator's ordinary
//! deferred-free path.
//!
//! [RFC 0046]: ../../../rfcs/0046-directory-deltas-in-the-window.md

use std::collections::BTreeMap;

use wavedb_core::wire::{WaveWire, from_wire_checked, to_wire_checked};

use crate::block::{BlockDescriptor, Run};
use crate::block_file::BlockFile;
use crate::error::{StorageError, StorageResult};
use crate::page_store::PageStore;
use crate::plan::SlotPlan;
use crate::struct_storage::StructStorage;

/// Edit-chunk blocks that must accumulate before rewriting the snapshot is
/// worth it. Without a floor a small database would snapshot every round.
const COMPACT_FLOOR_BLOCKS: u64 = 16; // 64 KiB

/// Hard cap on chunks between snapshots — bounds a recovery's scattered reads
/// even when the snapshot is large enough that the ratio alone would not.
const MAX_EDIT_CHUNKS: usize = 1024;

/// One type's descriptor changes in a round.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub struct SlotEdit {
    pub struct_hash: u64,
    /// Bucket count *after* the round: replay grows the vector to it, so a
    /// linear-hashing split needs no record of its own.
    pub buckets: u32,
    /// `(bucket, raw BlockDescriptor)` for every bucket that moved.
    pub changed: Vec<(u32, u64)>,
    /// The dictionary's run descriptor — `Some` only when it changed.
    pub dict: Option<u64>,
}

/// What one settle window changed.
///
/// A **full** chunk names every bucket of every type, so applying it over any
/// prior state yields exactly that state; nothing on the wire distinguishes it
/// from a delta, and nothing needs to.
#[derive(Debug, Clone, PartialEq, Eq, WaveWire)]
pub struct EditChunk {
    pub slots: Vec<SlotEdit>,
}

/// Serialise a chunk for the window: `[len u32 LE][crc32][wire]`, the same
/// self-delimiting envelope a page uses (a run is block-padded, so the length
/// prefix is what bounds the decode).
pub fn encode(chunk: &EditChunk) -> Vec<u8> {
    let payload = to_wire_checked(chunk);
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&payload);
    out
}

/// Read one chunk back from the run a `Commit` frame named.
///
/// # Errors
/// [`StorageError::Io`] on a read fault; [`StorageError::Corrupt`] if the
/// envelope, its crc, or its payload does not decode — a frame naming a run
/// asserts that run is durable, so a bad chunk is corruption, not a torn tail.
pub fn read_chunk(file: &BlockFile, run: Run) -> StorageResult<EditChunk> {
    let bytes = file.read_run(run)?;
    let len = bytes
        .get(..4)
        .and_then(|p| p.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(StorageError::Corrupt("edit chunk header"))?
        as usize;
    let body = bytes
        .get(4..4 + len)
        .ok_or(StorageError::Corrupt("edit chunk length"))?;
    from_wire_checked(body).map_err(|_| StorageError::Corrupt("edit chunk"))
}

/// Build the chunk describing `plans`.
///
/// `dicts` maps a plan's index to the dictionary descriptor it was just given;
/// on the reservation pass it is empty and the placeholder keeps the shape.
pub fn chunk_of(
    store: &PageStore,
    plans: &[SlotPlan],
    full: bool,
    dicts: &BTreeMap<usize, BlockDescriptor>,
) -> EditChunk {
    let mut slots: Vec<SlotEdit> = plans
        .iter()
        .enumerate()
        .map(|(plan, work)| planned_edit(store, plan, work, full, dicts))
        .collect();
    if full {
        // A snapshot must stand alone, so the types this round did not touch
        // contribute their resident state.
        for (idx, slot) in store.types.iter().enumerate() {
            if plans.iter().any(|work| work.idx == idx) {
                continue;
            }
            if let Some(edit) = resident_edit(slot) {
                slots.push(edit);
            }
        }
    }
    EditChunk { slots }
}

/// The edit for a slot this round planned — its working directory holds the
/// old descriptors on the reservation pass and the new ones after `install`.
fn planned_edit(
    store: &PageStore,
    plan: usize,
    work: &SlotPlan,
    full: bool,
    dicts: &BTreeMap<usize, BlockDescriptor>,
) -> SlotEdit {
    let slot = store.types[work.idx];
    let changed = if full {
        every_bucket(work.dir.slots())
    } else {
        work.pages
            .keys()
            .map(|&bucket| (bucket as u32, work.dir.descriptor(bucket).raw()))
            .collect()
    };
    let dict = if work.dict.is_some() {
        Some(dicts.get(&plan).copied().map_or(0, BlockDescriptor::raw))
    } else if full {
        Some(slot.dictionary().lock().descriptor().raw())
    } else {
        None
    };
    SlotEdit {
        struct_hash: slot.struct_hash(),
        buckets: work.dir.len() as u32,
        changed,
        dict,
    }
}

/// A snapshot entry for a type this round did not touch.
fn resident_edit(slot: &'static StructStorage) -> Option<SlotEdit> {
    // The directory guard is released before the dictionary's is taken: this
    // is a read of settled state, so it needs no consistent pair.
    let guard = slot.directory().lock();
    let dir = guard.as_ref()?;
    let buckets = dir.len() as u32;
    let changed = every_bucket(dir.slots());
    drop(guard);
    Some(SlotEdit {
        struct_hash: slot.struct_hash(),
        buckets,
        changed,
        dict: Some(slot.dictionary().lock().descriptor().raw()),
    })
}

fn every_bucket(slots: &[BlockDescriptor]) -> Vec<(u32, u64)> {
    slots
        .iter()
        .enumerate()
        .map(|(bucket, desc)| (bucket as u32, desc.raw()))
        .collect()
}

/// The addressing state rebuilt by replaying a snapshot and the chunks after
/// it, in order.
#[derive(Debug, Default)]
pub struct Replay {
    slots: BTreeMap<u64, Vec<u64>>,
    dicts: BTreeMap<u64, u64>,
}

impl Replay {
    /// Fold one chunk in.
    ///
    /// # Errors
    /// [`StorageError::Corrupt`] if a chunk names a bucket outside the
    /// directory size it declares.
    pub fn apply(&mut self, chunk: &EditChunk) -> StorageResult<()> {
        for edit in &chunk.slots {
            let addresses = self.slots.entry(edit.struct_hash).or_default();
            if (edit.buckets as usize) > addresses.len() {
                addresses.resize(edit.buckets as usize, 0);
            }
            for &(bucket, raw) in &edit.changed {
                let cell = addresses.get_mut(bucket as usize).ok_or(
                    StorageError::Corrupt("edit chunk bucket out of range"),
                )?;
                *cell = raw;
            }
            if let Some(dict) = edit.dict {
                self.dicts.insert(edit.struct_hash, dict);
            }
        }
        Ok(())
    }

    /// `(STRUCT_HASH → bucket descriptors, STRUCT_HASH → dictionary run)`.
    pub fn into_parts(self) -> (BTreeMap<u64, Vec<u64>>, BTreeMap<u64, u64>) {
        (self.slots, self.dicts)
    }
}

/// The log of addressing records currently on disk: the last full snapshot and
/// every delta chunk written since it. A `Commit` frame is this, verbatim.
#[derive(Debug, Default)]
pub struct MetaLog {
    snapshot: BlockDescriptor,
    edits: Vec<BlockDescriptor>,
    edit_blocks: u64,
}

impl MetaLog {
    /// The log a recovered `Commit` frame described.
    pub fn restored(
        snapshot: BlockDescriptor,
        edits: Vec<BlockDescriptor>,
    ) -> Self {
        let edit_blocks = edits.iter().map(|d| d.count()).sum();
        Self {
            snapshot,
            edits,
            edit_blocks,
        }
    }

    /// Whether the next round should write a full snapshot instead of a delta:
    /// once the deltas outweigh the state they patch, rewriting it costs less
    /// than carrying them.
    pub fn wants_snapshot(&self) -> bool {
        self.edits.len() >= MAX_EDIT_CHUNKS
            || self.edit_blocks
                >= self.snapshot.count().max(COMPACT_FLOOR_BLOCKS)
    }

    /// Record the round's chunk, returning the runs it superseded (a snapshot
    /// supersedes the previous snapshot and every chunk after it).
    pub fn record(&mut self, chunk: BlockDescriptor, full: bool) -> Vec<Run> {
        if !full {
            self.edit_blocks += chunk.count();
            self.edits.push(chunk);
            return Vec::new();
        }
        let mut stale: Vec<Run> =
            self.edits.drain(..).map(BlockDescriptor::run).collect();
        if self.snapshot.is_allocated() {
            stale.push(self.snapshot.run());
        }
        self.edit_blocks = 0;
        self.snapshot = chunk;
        stale
    }

    /// Every run the log occupies — part of a checkpoint's protected set.
    pub fn runs(&self) -> Vec<Run> {
        self.edits
            .iter()
            .chain(std::iter::once(&self.snapshot))
            .filter(|d| d.is_allocated())
            .map(|d| d.run())
            .collect()
    }

    /// `(snapshot, edits)` as the raw descriptors a `Commit` frame carries.
    pub fn frame(&self) -> (u64, Vec<u64>) {
        (
            self.snapshot.raw(),
            self.edits.iter().map(|d| d.raw()).collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{EditChunk, MetaLog, Replay, SlotEdit, encode};
    use crate::block::BlockDescriptor;
    use wavedb_core::wire::from_wire_checked;

    fn edit(hash: u64, buckets: u32, changed: &[(u32, u64)]) -> SlotEdit {
        SlotEdit {
            struct_hash: hash,
            buckets,
            changed: changed.to_vec(),
            dict: None,
        }
    }

    #[test]
    fn chunk_roundtrips_through_its_envelope() {
        let chunk = EditChunk {
            slots: vec![SlotEdit {
                struct_hash: 7,
                buckets: 3,
                changed: vec![(0, 111), (2, 333)],
                dict: Some(99),
            }],
        };
        let bytes = encode(&chunk);
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        assert_eq!(
            from_wire_checked::<EditChunk>(&bytes[4..4 + len]).unwrap(),
            chunk
        );
    }

    /// The reservation pass must predict the final pass exactly: same shape,
    /// same encoded length, whatever addresses end up in it.
    #[test]
    fn encoded_length_depends_on_shape_not_addresses() {
        let skeleton = EditChunk {
            slots: vec![edit(7, 3, &[(0, 0), (2, 0)])],
        };
        let filled = EditChunk {
            slots: vec![edit(7, 3, &[(0, u64::MAX), (2, 0x1234_5678)])],
        };
        assert_eq!(encode(&skeleton).len(), encode(&filled).len());
    }

    #[test]
    fn replay_folds_deltas_over_a_snapshot() {
        let mut replay = Replay::default();
        replay
            .apply(&EditChunk {
                slots: vec![SlotEdit {
                    struct_hash: 7,
                    buckets: 2,
                    changed: vec![(0, 10), (1, 20)],
                    dict: Some(5),
                }],
            })
            .unwrap();
        // A delta: bucket 1 moves, bucket 2 is new, the dictionary is untouched.
        replay
            .apply(&EditChunk {
                slots: vec![edit(7, 3, &[(1, 21), (2, 30)])],
            })
            .unwrap();
        let (slots, dicts) = replay.into_parts();
        assert_eq!(slots[&7], vec![10, 21, 30]);
        assert_eq!(dicts[&7], 5, "an untouched dictionary must carry forward");
    }

    #[test]
    fn a_bucket_outside_the_directory_is_corruption() {
        let mut replay = Replay::default();
        assert!(
            replay
                .apply(&EditChunk {
                    slots: vec![edit(7, 2, &[(5, 10)])],
                })
                .is_err()
        );
    }

    #[test]
    fn compaction_triggers_on_the_floor_then_on_the_ratio() {
        let mut log = MetaLog::default();
        assert!(!log.wants_snapshot(), "an empty log needs no snapshot");
        // Below the floor a small database keeps appending deltas.
        for start in 0..15 {
            log.record(BlockDescriptor::new(start, 1, 0), false);
        }
        assert!(!log.wants_snapshot());
        log.record(BlockDescriptor::new(100, 1, 0), false);
        assert!(log.wants_snapshot(), "16 blocks of deltas hits the floor");

        // Snapshotting frees everything before it and resets the count.
        let stale = log.record(BlockDescriptor::new(200, 64, 0), true);
        assert_eq!(stale.len(), 16, "every delta chunk is superseded");
        assert!(!log.wants_snapshot());
        assert_eq!(log.runs().len(), 1, "only the snapshot is live");

        // Now the ratio rules: 64 blocks of snapshot want 64 of deltas.
        for start in 0..63 {
            log.record(BlockDescriptor::new(1000 + start, 1, 0), false);
        }
        assert!(!log.wants_snapshot());
        log.record(BlockDescriptor::new(2000, 1, 0), false);
        assert!(log.wants_snapshot());
    }

    #[test]
    fn frame_carries_the_snapshot_and_every_delta() {
        let mut log = MetaLog::default();
        log.record(BlockDescriptor::new(10, 2, 0), true);
        log.record(BlockDescriptor::new(20, 1, 0), false);
        let (snapshot, edits) = log.frame();
        assert_eq!(snapshot, BlockDescriptor::new(10, 2, 0).raw());
        assert_eq!(edits, vec![BlockDescriptor::new(20, 1, 0).raw()]);
        // And a restored log resumes with the same accounting.
        let restored = MetaLog::restored(
            BlockDescriptor::from_raw(snapshot),
            edits
                .iter()
                .map(|&r| BlockDescriptor::from_raw(r))
                .collect(),
        );
        assert_eq!(restored.frame(), log.frame());
        assert_eq!(restored.runs().len(), 2);
    }
}
