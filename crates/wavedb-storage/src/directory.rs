//! The per-`STRUCT_HASH` page directory and its **linear hashing** addressing.
//!
//! Each `STRUCT_HASH` owns a `Vec<BlockDescriptor>` — one slot ("bucket") per
//! homogeneous page. Records are routed to a bucket by [`hash_of`] reduced through
//! [`bucket_index`]. Linear hashing grows the directory **one bucket at a time**,
//! so a grow rehashes only a single bucket, never the whole type.
//!
//! This module owns the **addressing math** and the directory container. The
//! page-moving half of a split (`split_next`) needs the block I/O layer and lands
//! with it; [`Directory::next_split_bucket`] exposes which bucket is next in line.
//!
//! ## The id hash
//!
//! `hash_of` is **SeaHash** over the `Id`'s 16 little-endian bytes. The seed is a
//! per-database random `[u64; 4]` (persisted in `data.bin` page 0), which gives the
//! DoS resistance (an attacker can't precompute bucket collisions). SeaHash is
//! portable across architecture and endianness, so the directory — rebuilt by
//! **journal replay** — routes every record to the same bucket even if `data.bin`
//! is opened on a different machine.

use crate::block::{BlockAllocator, BlockDescriptor};
use crate::block_file::BlockFile;
use crate::error::StorageResult;
use crate::page::SlotPage;
use seahash::hash_seeded;
use wavedb_core::Id;

/// A SeaHash of a 128-bit `Id` under a per-database seed.
///
/// Portable across every machine, architecture, and endianness (the `seahash`
/// crate guarantees it), so a `data.bin` rebuilt by journal replay on a different
/// machine routes every record to the same bucket. The `Id` is fed as its 16
/// little-endian bytes; the per-database `seed` randomises bucket placement so
/// collisions can't be precomputed.
#[must_use]
pub fn hash_of(id: u128, seed: [u64; 4]) -> u64 {
    hash_seeded(&id.to_le_bytes(), seed[0], seed[1], seed[2], seed[3])
}

/// Reduce a hash to a bucket index over a directory of `dir_len` buckets, using
/// **linear hashing** (not `hash % len`).
///
/// `dir_len` must be `>= 1`. The result is always `< dir_len`.
#[must_use]
pub fn bucket_index(dir_len: u64, hash: u64) -> usize {
    debug_assert!(dir_len >= 1, "directory must have at least one bucket");
    let level = dir_len.ilog2();
    let base = 1u64 << level; // largest power of two <= dir_len
    let split = dir_len - base; // buckets [0, split) have already been split this round
    let mut b = hash & (base - 1);
    if b < split {
        // This bucket was split: use one more bit to choose the two halves.
        b = hash & ((base << 1) - 1);
    }
    debug_assert!(b < dir_len);
    b as usize
}

/// The bucket that the next [`Directory`] grow will split (round-robin within the
/// current level).
#[must_use]
pub const fn next_split_bucket(dir_len: u64) -> u64 {
    let level = dir_len.ilog2();
    dir_len - (1u64 << level)
}

/// A per-`STRUCT_HASH` page directory: a vector of [`BlockDescriptor`] slots plus
/// the per-database hash seed.
#[derive(Debug, Clone)]
pub struct Directory {
    /// One descriptor per bucket (page).
    slots: Vec<BlockDescriptor>,
    /// Per-database hash seed (from `data.bin` page 0).
    seed: [u64; 4],
}

impl Directory {
    /// A directory with a single empty bucket.
    #[must_use]
    pub fn new(seed: [u64; 4]) -> Self {
        Self {
            slots: vec![BlockDescriptor::EMPTY],
            seed,
        }
    }

    /// Rebuild a directory from already-known descriptors (e.g. journal replay).
    ///
    /// # Panics
    /// Panics if `slots` is empty — a directory always has at least one bucket.
    #[must_use]
    pub fn from_slots(slots: Vec<BlockDescriptor>, seed: [u64; 4]) -> Self {
        assert!(!slots.is_empty(), "directory needs at least one bucket");
        Self { slots, seed }
    }

    /// Number of buckets.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.slots.len()
    }

    /// Always `false` — a directory always has at least one bucket.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// The per-database hash seed.
    #[must_use]
    pub const fn seed(&self) -> [u64; 4] {
        self.seed
    }

    /// Hash an `Id` (its raw `u128`) under this directory's seed.
    #[must_use]
    pub fn hash(&self, id: u128) -> u64 {
        hash_of(id, self.seed)
    }

    /// The bucket index an `Id` routes to.
    #[must_use]
    pub fn bucket_of(&self, id: u128) -> usize {
        bucket_index(self.slots.len() as u64, self.hash(id))
    }

    /// The descriptor at `bucket`.
    #[must_use]
    pub fn descriptor(&self, bucket: usize) -> BlockDescriptor {
        self.slots[bucket]
    }

    /// Replace the descriptor at `bucket`.
    pub fn set_descriptor(&mut self, bucket: usize, desc: BlockDescriptor) {
        self.slots[bucket] = desc;
    }

    /// Append a new bucket (the directory-growth half of a split) and return its
    /// index.
    pub fn push_bucket(&mut self, desc: BlockDescriptor) -> usize {
        self.slots.push(desc);
        self.slots.len() - 1
    }

    /// Which bucket the next grow will split.
    #[must_use]
    pub const fn next_split_bucket(&self) -> u64 {
        next_split_bucket(self.slots.len() as u64)
    }

    /// The bit position used to partition keys when splitting the current bucket
    /// (`= level`). On a split, a key stays in the source bucket when this bit is
    /// `0` and moves to the new bucket when it is `1`.
    #[must_use]
    pub const fn split_bit(&self) -> u32 {
        (self.slots.len() as u64).ilog2()
    }

    /// All descriptors, in bucket order.
    #[must_use]
    pub fn slots(&self) -> &[BlockDescriptor] {
        &self.slots
    }

    // ---- Page I/O: routing records into bucket pages ------------------------

    /// Read the [`SlotPage`] backing `bucket`, or a fresh empty page if the slot
    /// is unallocated.
    ///
    /// # Errors
    /// [`StorageError::Io`](crate::StorageError::Io) on a read fault or
    /// [`StorageError::Corrupt`](crate::StorageError::Corrupt) if the page fails
    /// its crc / bounds checks.
    pub fn read_page(
        &self,
        struct_hash: u64,
        file: &BlockFile,
        bucket: usize,
    ) -> StorageResult<SlotPage> {
        let desc = self.slots[bucket];
        if !desc.is_allocated() {
            return Ok(SlotPage::new(struct_hash));
        }
        let page = SlotPage::from_bytes(&file.read_run(desc.run())?)?;
        debug_assert_eq!(page.struct_hash(), struct_hash);
        Ok(page)
    }

    /// The record bytes stored at `id`, if present.
    ///
    /// # Errors
    /// Propagates read / corruption faults from [`read_page`](Self::read_page).
    pub fn get_record(
        &self,
        struct_hash: u64,
        file: &BlockFile,
        id: Id,
    ) -> StorageResult<Option<Vec<u8>>> {
        let bucket = self.bucket_of(id.raw());
        Ok(self
            .read_page(struct_hash, file, bucket)?
            .get(id)
            .map(<[u8]>::to_vec))
    }

    /// Route `id` to its bucket, upsert its bytes, and rewrite the page.
    ///
    /// Crash-safe ordering: the new page is allocated and written **before** the
    /// directory slot is repointed, and the old run is freed only afterwards — so
    /// the slot never names a half-written page.
    ///
    /// # Errors
    /// Propagates read / write / corruption faults.
    pub fn upsert_record(
        &mut self,
        struct_hash: u64,
        file: &BlockFile,
        alloc: &mut BlockAllocator,
        id: Id,
        bytes: Vec<u8>,
    ) -> StorageResult<()> {
        let bucket = self.bucket_of(id.raw());
        let old = self.slots[bucket];
        let mut page = self.read_page(struct_hash, file, bucket)?;
        page.upsert(id, bytes);
        self.slots[bucket] = place(file, alloc, &page)?;
        if old.is_allocated() {
            alloc.free(old.run());
        }
        Ok(())
    }

    /// Remove `id` from its bucket page. Returns whether it was present.
    ///
    /// # Errors
    /// Propagates read / write / corruption faults.
    pub fn remove_record(
        &mut self,
        struct_hash: u64,
        file: &BlockFile,
        alloc: &mut BlockAllocator,
        id: Id,
    ) -> StorageResult<bool> {
        let bucket = self.bucket_of(id.raw());
        let old = self.slots[bucket];
        if !old.is_allocated() {
            return Ok(false);
        }
        let mut page = self.read_page(struct_hash, file, bucket)?;
        let existed = page.remove(id).is_some();
        self.slots[bucket] = place(file, alloc, &page)?;
        alloc.free(old.run());
        Ok(existed)
    }

    /// Split the next bucket in round-robin order, repartitioning its records by
    /// the next hash bit and appending one new bucket — the page-moving half of
    /// linear-hashing growth.
    ///
    /// # Errors
    /// Propagates read / write / corruption faults.
    pub fn split_next(
        &mut self,
        struct_hash: u64,
        file: &BlockFile,
        alloc: &mut BlockAllocator,
    ) -> StorageResult<()> {
        let level = self.split_bit();
        let s = self.next_split_bucket() as usize;
        let old = self.slots[s];

        let mut keep = SlotPage::new(struct_hash);
        let mut moved = SlotPage::new(struct_hash);
        for (id, bytes) in self.read_page(struct_hash, file, s)?.into_entries()
        {
            // Bit `level` decides: 0 stays in `s`, 1 moves to the new bucket.
            if (self.hash(id.raw()) >> level) & 1 == 0 {
                keep.upsert(id, bytes);
            } else {
                moved.upsert(id, bytes);
            }
        }

        let keep_desc = place(file, alloc, &keep)?;
        let moved_desc = place(file, alloc, &moved)?;
        self.slots[s] = keep_desc;
        self.slots.push(moved_desc); // new bucket at index M (= s + base)
        if old.is_allocated() {
            alloc.free(old.run());
        }
        Ok(())
    }
}

/// Allocate a run sized to `page`, write it, and return its descriptor. An empty
/// page allocates nothing and yields [`BlockDescriptor::EMPTY`].
fn place(
    file: &BlockFile,
    alloc: &mut BlockAllocator,
    page: &SlotPage,
) -> StorageResult<BlockDescriptor> {
    if page.is_empty() {
        return Ok(BlockDescriptor::EMPTY);
    }
    let bytes = page.to_bytes();
    let run = alloc.alloc(page.blocks_needed());
    file.write_run(run, &bytes)?;
    Ok(BlockDescriptor::from_run(
        run,
        occupation_of(bytes.len() as u64, run.byte_len()),
    ))
}

/// Coarse 1/16th fill gauge: how full a page's bytes leave its allocated run.
fn occupation_of(used: u64, capacity: u64) -> u8 {
    if capacity == 0 {
        return 0;
    }
    (used.saturating_mul(16) / capacity).min(15) as u8
}

#[cfg(test)]
mod tests {
    use super::{Directory, bucket_index, hash_of, next_split_bucket};
    use crate::block::BlockDescriptor;

    const SEED: [u64; 4] = [0x1234, 0x5678, 0x9abc, 0xdef0];

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(hash_of(42, SEED), hash_of(42, SEED));
        assert_ne!(hash_of(42, SEED), hash_of(43, SEED));
        // Seed changes the result.
        assert_ne!(hash_of(42, SEED), hash_of(42, [9, 9, 9, 9]));
    }

    #[test]
    fn hash_covers_all_low_buckets() {
        // Sequential ids must spread across the low bucket bits, not concentrate
        // (the failure mode of a weak hash). With 4096 ids over 256 low-byte
        // values, a good hash hits every value; a degenerate one leaves gaps.
        let mut seen = std::collections::HashSet::new();
        for id in 0u128..4096 {
            seen.insert(hash_of(id, SEED) & 0xFF);
        }
        assert_eq!(
            seen.len(),
            256,
            "hash left low-byte buckets empty: {}",
            seen.len()
        );
    }

    #[test]
    fn bucket_index_in_range_for_all_sizes() {
        for dir_len in 1u64..=64 {
            for h in 0u64..1000 {
                let b =
                    bucket_index(dir_len, h.wrapping_mul(0x9E37_79B9)) as u64;
                assert!(b < dir_len, "bucket {b} >= len {dir_len}");
            }
        }
    }

    #[test]
    fn power_of_two_directory_is_plain_mask() {
        // When dir_len is a power of two, split == 0 → just the low `level` bits.
        for &len in &[1u64, 2, 4, 8, 16] {
            for h in 0u64..500 {
                assert_eq!(bucket_index(len, h) as u64, h & (len - 1));
            }
        }
    }

    #[test]
    fn split_preserves_linear_hashing_invariant() {
        // Growing from m to m+1 splits exactly bucket `s`: keys there move to `s`
        // or the new bucket `m`; every other key keeps its bucket.
        for m in 1u64..32 {
            let s = next_split_bucket(m);
            let new_bucket = m; // appended at index m
            for h in 0u64..2000 {
                let hash = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
                let before = bucket_index(m, hash) as u64;
                let after = bucket_index(m + 1, hash) as u64;
                if before == s {
                    assert!(
                        after == s || after == new_bucket,
                        "split bucket {s}: {before} -> {after} (m={m})"
                    );
                } else {
                    assert_eq!(
                        after, before,
                        "unaffected bucket changed (m={m})"
                    );
                }
            }
        }
    }

    #[test]
    fn directory_routing_and_growth() {
        let mut dir = Directory::new(SEED);
        assert_eq!(dir.len(), 1);
        assert!(!dir.is_empty());
        // Single bucket: everything routes to 0.
        assert_eq!(dir.bucket_of(123), 0);
        assert_eq!(dir.bucket_of(999), 0);

        dir.set_descriptor(0, BlockDescriptor::new(10, 4, 8));
        assert_eq!(dir.descriptor(0).run().start, 10);

        let idx = dir.push_bucket(BlockDescriptor::new(20, 4, 0));
        assert_eq!(idx, 1);
        assert_eq!(dir.len(), 2);
        // Now routing uses one bit → buckets 0 and 1 both reachable.
        let buckets: std::collections::HashSet<usize> =
            (0u128..100).map(|id| dir.bucket_of(id)).collect();
        assert_eq!(buckets, [0, 1].into_iter().collect());
    }

    #[test]
    fn next_split_bucket_round_robin() {
        // Level 1 (len 2..3): splits bucket 0, then 1, then wraps to level 2.
        assert_eq!(next_split_bucket(2), 0);
        assert_eq!(next_split_bucket(3), 1);
        assert_eq!(next_split_bucket(4), 0);
        assert_eq!(next_split_bucket(5), 1);
        assert_eq!(next_split_bucket(7), 3);
    }

    #[test]
    fn from_slots_roundtrips() {
        let slots =
            vec![BlockDescriptor::new(0, 1, 0), BlockDescriptor::new(8, 1, 0)];
        let dir = Directory::from_slots(slots.clone(), SEED);
        assert_eq!(dir.slots(), slots.as_slice());
        assert_eq!(dir.seed(), SEED);
        assert_eq!(dir.split_bit(), 1);
    }

    // ---- page-I/O over a real BlockFile -------------------------------------

    use crate::block::BlockAllocator;
    use crate::block_file::{BlockFile, RESERVED_BLOCKS};
    use wavedb_core::{Id, U48};

    /// A temp `data.bin` plus an allocator with block 0 (the superblock) reserved.
    fn backed() -> (tempfile::TempDir, BlockFile, BlockAllocator) {
        let dir = tempfile::tempdir().unwrap();
        let bf = BlockFile::open(dir.path().join("data.bin")).unwrap();
        let mut alloc = BlockAllocator::new();
        alloc.alloc(RESERVED_BLOCKS); // never hand out the superblock
        (dir, bf, alloc)
    }

    fn rec_id(key: u64) -> Id {
        Id::new(key, U48::from(1u32), false, (key & 0x7FFF) as u16)
    }

    #[test]
    fn upsert_get_remove_through_pages() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        let sh = 0xABCD;

        dir.upsert_record(sh, &bf, &mut alloc, rec_id(1), vec![1, 2, 3])
            .unwrap();
        dir.upsert_record(sh, &bf, &mut alloc, rec_id(2), vec![4, 5])
            .unwrap();
        assert_eq!(
            dir.get_record(sh, &bf, rec_id(1)).unwrap(),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            dir.get_record(sh, &bf, rec_id(2)).unwrap(),
            Some(vec![4, 5])
        );
        assert_eq!(dir.get_record(sh, &bf, rec_id(9)).unwrap(), None);

        // Overwrite, then remove.
        dir.upsert_record(sh, &bf, &mut alloc, rec_id(1), vec![9])
            .unwrap();
        assert_eq!(dir.get_record(sh, &bf, rec_id(1)).unwrap(), Some(vec![9]));
        assert!(dir.remove_record(sh, &bf, &mut alloc, rec_id(1)).unwrap());
        assert_eq!(dir.get_record(sh, &bf, rec_id(1)).unwrap(), None);
        assert!(!dir.remove_record(sh, &bf, &mut alloc, rec_id(1)).unwrap());
    }

    #[test]
    fn split_preserves_all_records() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        let sh = 1;

        for k in 0..200u64 {
            dir.upsert_record(sh, &bf, &mut alloc, rec_id(k), vec![k as u8; 4])
                .unwrap();
        }
        assert_eq!(dir.len(), 1);

        dir.split_next(sh, &bf, &mut alloc).unwrap();
        assert_eq!(dir.len(), 2);

        // Every record still resolves after the repartition.
        for k in 0..200u64 {
            assert_eq!(
                dir.get_record(sh, &bf, rec_id(k)).unwrap(),
                Some(vec![k as u8; 4]),
                "record {k} lost across split"
            );
        }
    }

    #[test]
    fn split_empty_bucket_is_noop_growth() {
        let (_d, bf, mut alloc) = backed();
        let mut dir = Directory::new(bf.seed());
        dir.split_next(1, &bf, &mut alloc).unwrap();
        assert_eq!(dir.len(), 2);
    }
}
