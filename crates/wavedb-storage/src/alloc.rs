//! [`BlockAllocator`] — the in-memory free-space manager over `data.bin`'s
//! block array (split from [`crate::block`] for the file budget).
//!
//! Free extents are tracked twice: by **position** (a `BTreeMap<start,
//! count>`, so a freed run coalesces with its neighbours in `O(log n)`) and
//! by **size** (a `BTreeSet<(count, start)>`, so allocation is best-fit).
//! `total_blocks` is the current file length in blocks; allocation that
//! can't be satisfied from a hole grows the file at the tail.
//!
//! ## Checkpoint protection
//!
//! Runs named by the last **durable checkpoint** must survive until the next
//! checkpoint commits — a crash in between reopens *from* that checkpoint,
//! so overwriting a run it points at would corrupt the reopened state.
//! [`set_protected`](BlockAllocator::set_protected) registers those runs;
//! freeing one is **deferred** (held in a pending list) and released when a
//! later `set_protected` drops its protection. With no checkpoint (a fresh
//! or rebuild-on-open store) nothing is protected and frees are immediate.

use std::collections::{BTreeMap, BTreeSet};

use crate::block::Run;

/// An in-memory free-space manager over `data.bin`'s block array.
#[derive(Debug, Default)]
pub struct BlockAllocator {
    /// start → count, for coalescing on free.
    by_pos: BTreeMap<u64, u64>,
    /// (count, start), for best-fit allocation.
    by_size: BTreeSet<(u64, u64)>,
    /// Current file length, in blocks.
    total_blocks: u64,
    /// start → count of the last durable checkpoint's runs (see module docs).
    protected: BTreeMap<u64, u64>,
    /// Frees deferred because the run was protected at the time.
    pending: Vec<Run>,
}

impl BlockAllocator {
    /// A fresh allocator over an empty file.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild an allocator from a persisted layout: the file length and the
    /// runs currently in use (reserved head, pages, dictionary runs, the
    /// checkpoint run itself). Every gap between used runs becomes a free
    /// extent. The used runs are **not** protected — call
    /// [`set_protected`](Self::set_protected) with the checkpoint's runs
    /// after.
    #[must_use]
    pub fn from_layout(total_blocks: u64, used: &[Run]) -> Self {
        let mut runs: Vec<Run> =
            used.iter().copied().filter(|r| r.count > 0).collect();
        runs.sort_by_key(|r| r.start);
        let mut a = Self {
            total_blocks,
            ..Self::default()
        };
        let mut cursor = 0u64;
        for run in runs {
            debug_assert!(run.start >= cursor, "used runs overlap");
            if run.start > cursor {
                a.insert_extent(cursor, run.start - cursor);
            }
            cursor = run.end();
        }
        if cursor < total_blocks {
            a.insert_extent(cursor, total_blocks - cursor);
        }
        a
    }

    /// Current file length, in blocks.
    #[must_use]
    pub const fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    /// Total free blocks currently held in extents (excludes the unallocated
    /// tail beyond `total_blocks` and any deferred frees).
    #[must_use]
    pub fn free_blocks(&self) -> u64 {
        self.by_size.iter().map(|&(count, _)| count).sum()
    }

    /// Number of distinct free extents (a fragmentation gauge, for
    /// tests/metrics).
    #[must_use]
    pub fn free_extent_count(&self) -> usize {
        self.by_pos.len()
    }

    /// Allocate a contiguous run of `count` blocks (best-fit; grows the file
    /// if no hole fits). `count` must be `>= 1`.
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

    /// Return a run to the free pool, coalescing with any adjacent free
    /// extents. A run the last durable checkpoint still points at is
    /// **deferred** instead (released by the next
    /// [`set_protected`](Self::set_protected)).
    ///
    /// # Panics
    /// Panics (in debug) if the run lies past the end of the file or
    /// overlaps an existing free extent (a double free).
    pub fn free(&mut self, run: Run) {
        if run.count == 0 {
            return;
        }
        if self.is_protected(run) {
            self.pending.push(run);
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

        // Coalesce with the successor extent if it starts exactly at the
        // run's end.
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

    /// Replace the protected set with the (new) checkpoint's runs, then
    /// retry every deferred free — anything no longer protected returns to
    /// the pool. Called right after a checkpoint's superblock pointer lands
    /// (the durability point that retires the previous checkpoint).
    pub fn set_protected(&mut self, runs: &[Run]) {
        self.protected = runs
            .iter()
            .filter(|r| r.count > 0)
            .map(|r| (r.start, r.count))
            .collect();
        let pending = std::mem::take(&mut self.pending);
        for run in pending {
            self.free(run); // re-checks protection
        }
    }

    /// Whether `run` intersects a protected run.
    fn is_protected(&self, run: Run) -> bool {
        // The candidate protected run is the last one starting at or before
        // `run`'s end; runs never overlap, so one probe suffices.
        self.protected
            .range(..run.end())
            .next_back()
            .is_some_and(|(&start, &count)| start + count > run.start)
    }

    /// Drop a free extent that reaches the end of the file, shrinking the
    /// file. Returns the number of blocks reclaimed (`0` if the tail isn't
    /// free).
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
    use super::BlockAllocator;
    use crate::block::Run;

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

    #[test]
    fn protected_frees_defer_until_released() {
        let mut a = BlockAllocator::new();
        let ckpt = a.alloc(2); // [0,2) — "the last checkpoint's run"
        let page = a.alloc(3); // [2,5)
        a.set_protected(&[ckpt]);

        a.free(ckpt); // protected → deferred
        assert_eq!(a.free_blocks(), 0, "protected free must defer");
        // The blocks stay unavailable: a new alloc grows the tail instead.
        assert_eq!(a.alloc(2), Run::new(5, 2));

        a.free(page); // unprotected → immediate
        assert_eq!(a.free_blocks(), 3);

        // The next checkpoint retires the old protection.
        a.set_protected(&[Run::new(5, 2)]);
        assert_eq!(a.free_blocks(), 5, "deferred free must release");
    }

    #[test]
    fn from_layout_frees_the_gaps() {
        // Used: [0,1) reserved, [3,5), [8,9). Total 10.
        let a = BlockAllocator::from_layout(
            10,
            &[Run::new(0, 1), Run::new(3, 2), Run::new(8, 1)],
        );
        assert_eq!(a.total_blocks(), 10);
        // Gaps: [1,3), [5,8), [9,10) = 2 + 3 + 1 free blocks.
        assert_eq!(a.free_blocks(), 6);
        assert_eq!(a.free_extent_count(), 3);
        let mut a = a;
        // Best fit for 2 is the exact [1,3) hole.
        assert_eq!(a.alloc(2), Run::new(1, 2));
    }
}
