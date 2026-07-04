//! The block layer: the 64-bit [`BlockDescriptor`], a [`Run`] of contiguous
//! blocks, and the in-memory [`BlockAllocator`].
//!
//! `data.bin` is an array of fixed [`BLOCK_SIZE`]-byte blocks. The allocator hands
//! out contiguous runs and reclaims them, coalescing adjacent free space so large
//! pages always have somewhere to land. It is a **pure in-memory structure** —
//! durability (journaling every alloc/free) is the pipeline's job, not this
//! module's.

use wavedb_core::wire::{Cursor, Result, WaveWire};

/// Size of one block in bytes (the allocation unit).
pub const BLOCK_SIZE: usize = 4096;

const START_BITS: u32 = 40;
const COUNT_BITS: u32 = 20;
const OCC_BITS: u32 = 4;

const START_SHIFT: u32 = COUNT_BITS + OCC_BITS; // 24
const COUNT_SHIFT: u32 = OCC_BITS; // 4

const START_MASK: u64 = (1 << START_BITS) - 1;
const COUNT_MASK: u64 = (1 << COUNT_BITS) - 1;
const OCC_MASK: u64 = (1 << OCC_BITS) - 1;

/// Largest representable start block (`2^40 − 1`).
pub const MAX_START: u64 = START_MASK;
/// Largest representable block count in one run/page (`2^20 − 1`).
pub const MAX_COUNT: u64 = COUNT_MASK;
/// Full occupation gauge value (16/16).
pub const MAX_OCCUPATION: u8 = OCC_MASK as u8;

/// A contiguous run of blocks: a start block and a length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Run {
    /// First block index.
    pub start: u64,
    /// Number of contiguous blocks (`>= 1` for a real run).
    pub count: u64,
}

impl Run {
    /// Build a run.
    #[must_use]
    pub const fn new(start: u64, count: u64) -> Self {
        Self { start, count }
    }

    /// One past the last block (`start + count`).
    #[must_use]
    pub const fn end(self) -> u64 {
        self.start + self.count
    }

    /// Byte offset of this run's first block in `data.bin`.
    #[must_use]
    pub const fn byte_offset(self) -> u64 {
        self.start * BLOCK_SIZE as u64
    }

    /// Total byte length of this run.
    #[must_use]
    pub const fn byte_len(self) -> u64 {
        self.count * BLOCK_SIZE as u64
    }
}

/// A 64-bit page/dictionary descriptor: `start (u40) · count (u20) · occupation
/// (u4)`. One format for both pages and dictionary runs.
///
/// `occupation` is a coarse 1/16th fill gauge the directory can read **without
/// touching the page** — enough to decide "grow / split" from the directory alone.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BlockDescriptor(u64);

impl BlockDescriptor {
    /// The empty descriptor — an unallocated directory slot.
    pub const EMPTY: Self = Self(0);

    /// Pack a descriptor.
    ///
    /// # Panics
    /// Panics (in debug) if any field exceeds its bit width.
    #[must_use]
    pub fn new(start: u64, count: u64, occupation: u8) -> Self {
        debug_assert!(start <= MAX_START, "start {start} exceeds u40");
        debug_assert!(count <= MAX_COUNT, "count {count} exceeds u20");
        debug_assert!(
            u64::from(occupation) <= OCC_MASK,
            "occupation {occupation} exceeds u4"
        );
        Self(
            ((start & START_MASK) << START_SHIFT)
                | ((count & COUNT_MASK) << COUNT_SHIFT)
                | (u64::from(occupation) & OCC_MASK),
        )
    }

    /// Pack a descriptor from a [`Run`] and an occupation gauge.
    #[must_use]
    pub fn from_run(run: Run, occupation: u8) -> Self {
        Self::new(run.start, run.count, occupation)
    }

    /// Pack a descriptor from a [`Run`] and the bytes actually used of it —
    /// the occupation gauge computed via [`occupation_of`].
    #[must_use]
    pub fn from_run_used(run: Run, used_bytes: u64) -> Self {
        Self::from_run(run, occupation_of(used_bytes, run.byte_len()))
    }

    /// Wrap a raw `u64` (as read from a directory `Vec<u64>`).
    #[must_use]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw `u64`.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// First block of the run.
    #[must_use]
    pub const fn start(self) -> u64 {
        (self.0 >> START_SHIFT) & START_MASK
    }

    /// Number of blocks in the run.
    #[must_use]
    pub const fn count(self) -> u64 {
        (self.0 >> COUNT_SHIFT) & COUNT_MASK
    }

    /// The 1/16th occupation gauge (0 = empty, 15 = full).
    #[must_use]
    pub const fn occupation(self) -> u8 {
        (self.0 & OCC_MASK) as u8
    }

    /// `true` if this slot points at a real run (`count > 0`).
    #[must_use]
    pub const fn is_allocated(self) -> bool {
        self.count() != 0
    }

    /// The [`Run`] this descriptor addresses.
    #[must_use]
    pub const fn run(self) -> Run {
        Run::new(self.start(), self.count())
    }

    /// A copy with a new occupation gauge (start/count unchanged).
    #[must_use]
    pub fn with_occupation(self, occupation: u8) -> Self {
        Self::new(self.start(), self.count(), occupation)
    }
}

/// Coarse 1/16th fill gauge: how full `used` bytes leave a `capacity`-byte run.
#[must_use]
pub fn occupation_of(used: u64, capacity: u64) -> u8 {
    if capacity == 0 {
        return 0;
    }
    (used.saturating_mul(16) / capacity).min(15) as u8
}

impl core::fmt::Debug for BlockDescriptor {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BlockDescriptor")
            .field("start", &self.start())
            .field("count", &self.count())
            .field("occupation", &self.occupation())
            .finish()
    }
}

impl WaveWire for BlockDescriptor {
    const STACK_SIZE: usize = 8;
    fn heap_size(&self) -> usize {
        0
    }
    fn encode_stack(&self, stack: &mut Vec<u8>) {
        stack.extend_from_slice(&self.0.to_le_bytes());
    }
    fn encode_heap(&self, _heap: &mut Vec<u8>) {}
    fn decode(stack: &mut Cursor, _heap: &mut Cursor) -> Result<Self> {
        Ok(Self(u64::from_le_bytes(stack.take(8)?.try_into().unwrap())))
    }
}

/// An in-memory free-space manager over `data.bin`'s block array.
///
/// Free extents are tracked twice: by **position** (a `BTreeMap<start, count>`, so
/// a freed run coalesces with its neighbours in `O(log n)`) and by **size** (a
/// `BTreeSet<(count, start)>`, so allocation is best-fit). `total_blocks` is the
/// current file length in blocks; allocation that can't be satisfied from a hole
/// grows the file at the tail.
#[derive(Debug, Default)]
pub struct BlockAllocator {
    /// start → count, for coalescing on free.
    by_pos: std::collections::BTreeMap<u64, u64>,
    /// (count, start), for best-fit allocation.
    by_size: std::collections::BTreeSet<(u64, u64)>,
    /// Current file length, in blocks.
    total_blocks: u64,
}

impl BlockAllocator {
    /// A fresh allocator over an empty file.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current file length, in blocks.
    #[must_use]
    pub const fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    /// Total free blocks currently held in extents (excludes the unallocated tail
    /// beyond `total_blocks`).
    #[must_use]
    pub fn free_blocks(&self) -> u64 {
        self.by_size.iter().map(|&(count, _)| count).sum()
    }

    /// Number of distinct free extents (a fragmentation gauge, for tests/metrics).
    #[must_use]
    pub fn free_extent_count(&self) -> usize {
        self.by_pos.len()
    }

    /// Allocate a contiguous run of `count` blocks (best-fit; grows the file if no
    /// hole fits). `count` must be `>= 1`.
    ///
    /// # Panics
    /// Panics if `count == 0`.
    pub fn alloc(&mut self, count: u64) -> Run {
        assert!(count > 0, "cannot allocate a zero-length run");

        // Best fit: the smallest free extent that can hold `count`.
        if let Some(&(extent_count, start)) =
            self.by_size.range((count, 0)..).next()
        {
            self.remove_extent(start, extent_count);
            let leftover = extent_count - count;
            if leftover > 0 {
                self.insert_extent(start + count, leftover);
            }
            return Run::new(start, count);
        }

        // No hole fits — grow the file at the tail.
        let start = self.total_blocks;
        self.total_blocks += count;
        Run::new(start, count)
    }

    /// Return a run to the free pool, coalescing with any adjacent free extents.
    ///
    /// # Panics
    /// Panics (in debug) if the run lies past the end of the file or overlaps an
    /// existing free extent (a double free).
    pub fn free(&mut self, run: Run) {
        if run.count == 0 {
            return;
        }
        debug_assert!(
            run.end() <= self.total_blocks,
            "freeing past end of file: {run:?} vs total {}",
            self.total_blocks
        );

        let mut start = run.start;
        let mut count = run.count;

        // Coalesce with the predecessor extent if it ends exactly at `start`.
        if let Some((&p_start, &p_count)) =
            self.by_pos.range(..start).next_back()
        {
            debug_assert!(
                p_start + p_count <= start,
                "double free overlaps predecessor"
            );
            if p_start + p_count == start {
                self.remove_extent(p_start, p_count);
                start = p_start;
                count += p_count;
            }
        }

        // Coalesce with the successor extent if it starts exactly at the run's end.
        let succ_start = start + count;
        if let Some(&succ_count) = self.by_pos.get(&succ_start) {
            self.remove_extent(succ_start, succ_count);
            count += succ_count;
        }
        debug_assert!(
            self.by_pos.range(start..start + count).next().is_none(),
            "double free overlaps successor"
        );

        self.insert_extent(start, count);
    }

    /// Drop a free extent that reaches the end of the file, shrinking the file.
    /// Returns the number of blocks reclaimed (`0` if the tail isn't free).
    pub fn truncate(&mut self) -> u64 {
        let Some((&start, &count)) = self.by_pos.iter().next_back() else {
            return 0;
        };
        if start + count == self.total_blocks {
            self.remove_extent(start, count);
            self.total_blocks = start;
            count
        } else {
            0
        }
    }

    fn insert_extent(&mut self, start: u64, count: u64) {
        self.by_pos.insert(start, count);
        self.by_size.insert((count, start));
    }

    fn remove_extent(&mut self, start: u64, count: u64) {
        self.by_pos.remove(&start);
        self.by_size.remove(&(count, start));
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockAllocator, BlockDescriptor, MAX_COUNT, MAX_START, Run};
    use wavedb_core::wire::{from_wire, to_wire};

    #[test]
    fn descriptor_packs_and_unpacks() {
        let d = BlockDescriptor::new(0x12_3456_789A, 0xABCDE, 0xF);
        assert_eq!(d.start(), 0x12_3456_789A);
        assert_eq!(d.count(), 0xABCDE);
        assert_eq!(d.occupation(), 0xF);
        assert!(d.is_allocated());
        assert_eq!(d.run(), Run::new(0x12_3456_789A, 0xABCDE));
    }

    #[test]
    fn descriptor_boundaries() {
        let d = BlockDescriptor::new(MAX_START, MAX_COUNT, 15);
        assert_eq!(d.start(), MAX_START);
        assert_eq!(d.count(), MAX_COUNT);
        assert_eq!(d.occupation(), 15);
        assert!(!BlockDescriptor::EMPTY.is_allocated());
        assert_eq!(BlockDescriptor::EMPTY.count(), 0);
    }

    #[test]
    fn descriptor_with_occupation_preserves_run() {
        let d = BlockDescriptor::new(100, 8, 3).with_occupation(12);
        assert_eq!(d.run(), Run::new(100, 8));
        assert_eq!(d.occupation(), 12);
    }

    #[test]
    fn descriptor_wire_roundtrip() {
        let d = BlockDescriptor::new(777, 9, 5);
        let bytes = to_wire(&d);
        assert_eq!(bytes.len(), 8);
        assert_eq!(from_wire::<BlockDescriptor>(&bytes).unwrap(), d);
    }

    #[test]
    fn alloc_grows_the_file() {
        let mut a = BlockAllocator::new();
        assert_eq!(a.alloc(3), Run::new(0, 3));
        assert_eq!(a.alloc(2), Run::new(3, 2));
        assert_eq!(a.total_blocks(), 5);
        assert_eq!(a.free_blocks(), 0);
    }

    #[test]
    fn free_then_reuse_best_fit() {
        let mut a = BlockAllocator::new();
        let _r0 = a.alloc(3); // [0,3)
        let r1 = a.alloc(2); // [3,5)
        let _r2 = a.alloc(4); // [5,9)
        a.free(r1); // hole [3,5)
        assert_eq!(a.free_blocks(), 2);
        // Best-fit reuses the 2-block hole exactly.
        assert_eq!(a.alloc(2), Run::new(3, 2));
        assert_eq!(a.free_blocks(), 0);
        assert_eq!(a.total_blocks(), 9);
    }

    #[test]
    fn best_fit_picks_smallest_sufficient_hole() {
        let mut a = BlockAllocator::new();
        let big = a.alloc(5); // [0,5)
        let _gap = a.alloc(1); // [5,6) keeps holes apart
        let small = a.alloc(2); // [6,8)
        let _tail = a.alloc(1); // [8,9)
        a.free(big); // hole size 5 at 0
        a.free(small); // hole size 2 at 6
        // Allocating 2 should take the size-2 hole, not split the size-5 one.
        assert_eq!(a.alloc(2), Run::new(6, 2));
    }

    #[test]
    fn free_coalesces_neighbours() {
        let mut a = BlockAllocator::new();
        let r0 = a.alloc(3); // [0,3)
        let r1 = a.alloc(2); // [3,5)
        let r2 = a.alloc(1); // [5,6)
        a.free(r1); // [3,5)
        a.free(r2); // coalesces with [3,5) → [3,6)
        assert_eq!(a.free_extent_count(), 1);
        a.free(r0); // coalesces with [3,6) → [0,6)
        assert_eq!(a.free_extent_count(), 1);
        assert_eq!(a.free_blocks(), 6);
        // The whole coalesced extent satisfies a big request in place.
        assert_eq!(a.alloc(6), Run::new(0, 6));
    }

    #[test]
    fn truncate_reclaims_free_tail_only() {
        let mut a = BlockAllocator::new();
        let r0 = a.alloc(3); // [0,3)
        let r1 = a.alloc(2); // [3,5)
        a.free(r0); // non-tail hole [0,3)
        assert_eq!(a.truncate(), 0); // tail [3,5) is allocated
        assert_eq!(a.total_blocks(), 5);
        a.free(r1); // now [0,5) coalesced, reaches end
        assert_eq!(a.truncate(), 5);
        assert_eq!(a.total_blocks(), 0);
        assert_eq!(a.free_blocks(), 0);
    }

    #[test]
    fn alloc_reuse_keeps_remainder() {
        let mut a = BlockAllocator::new();
        let r = a.alloc(10); // [0,10)
        a.free(r);
        assert_eq!(a.alloc(4), Run::new(0, 4)); // remainder [4,10) stays free
        assert_eq!(a.free_blocks(), 6);
        assert_eq!(a.alloc(6), Run::new(4, 6));
        assert_eq!(a.free_blocks(), 0);
    }
}
