//! Phases 2–4 and 6 of a checkpoint ([RFC 0041]) — one allocation, one write,
//! then the in-memory swap that publishes the new pages.
//!
//! Every image [`crate::plan`] produced — the rewritten pages, a grown
//! dictionary, and (at commit time) each drifted directory chain — is placed
//! inside **one contiguous window**: the allocator's existing best-fit finds
//! the smallest free extent that holds the whole checkpoint (a recycled window
//! when one exists, the file's tail otherwise), the window is filled
//! block-aligned, and it reaches the disk as a single positioned write. Only
//! afterwards are the descriptor vectors swapped — so a reader either sees the
//! previous pages (still allocated, still valid: page writes are
//! copy-on-write) or the new ones, never a half-written run.
//!
//! Chain nodes link by block index, so the window is laid out in two steps:
//! sizes first (a chain's node count follows from its bucket count alone),
//! then — once every image knows where it lands — the chain images are built
//! against the addresses the pages just received.
//!
//! Nothing on disk records that these blocks were written together: the window
//! is an allocation-time concept only, and each page keeps its own descriptor.
//! That is what lets pages be freed one by one later and the extent coalesce
//! back into a large hole.
//!
//! [RFC 0041]: ../../../rfcs/0041-single-barrier-checkpoint.md

use crate::block::{BLOCK_SIZE, BlockAllocator, BlockDescriptor, Run};
use crate::chain;
use crate::error::StorageResult;
use crate::page_store::{PageStore, Touched};
use crate::plan::{SlotPlan, blocks_of};

/// Where the window itself is allocated.
#[derive(Clone, Copy)]
enum Placement {
    /// The allocator's best fit: a recycled hole when one holds the whole
    /// checkpoint, the tail otherwise. Every ordinary settle and checkpoint.
    BestFit,
    /// Always fresh tail blocks — the defragmenter, which must not land back
    /// in the neighbourhood it is trying to vacate.
    Tail,
}

/// Where a placed image's descriptor is installed once the window is written.
enum Target {
    Page { plan: usize, bucket: usize },
    Dict { plan: usize },
    Chain { plan: usize },
}

impl PageStore {
    /// Settle `touched` into pages: plan every slot, then write the whole
    /// round as one window and publish it. Directory chains are **not**
    /// persisted here — a settle round leaves them dirty for the next
    /// checkpoint.
    ///
    /// Durability is not taken here either: the journal already holds the
    /// batch, and `data.bin` only becomes authoritative at
    /// [`commit_journal`](Self::commit_journal), which syncs the window before
    /// naming its blocks in a `Commit` frame.
    ///
    /// # Errors
    /// Propagates page read/write, corruption, or compression faults. The
    /// caller returns the round to the pending queue; re-settling is
    /// idempotent because a plan derives from the caches' current bytes.
    pub(crate) fn settle(&self, touched: &Touched) -> StorageResult<()> {
        let plans = self.plan_round(touched)?;
        self.place(plans)
    }

    /// A checkpoint's whole disk footprint in one window: every pending page
    /// **and** a fresh copy-on-write chain for every directory that drifted
    /// from its persisted one.
    ///
    /// # Errors
    /// As [`settle`](Self::settle); a failed round returns to the queue.
    pub(crate) fn settle_and_persist_chains(&self) -> StorageResult<()> {
        // Everything the retiring journal covers is already queued: `apply`
        // pushes under the journal lock, which the rotation also took.
        let round = std::mem::take(&mut *self.pending.lock());
        let mut plans = match self.plan_round(&round) {
            Ok(plans) => plans,
            Err(e) => {
                crate::settle::requeue(&mut self.pending.lock(), round);
                return Err(e);
            }
        };
        // A planned slot always drifts; the rest only if an earlier round
        // moved them.
        for (idx, slot) in self.types.iter().enumerate() {
            if plans.iter().any(|p| p.idx == idx) {
                continue;
            }
            let drifted = slot.chain().lock().dirty;
            let dir = slot.directory().lock().clone();
            if let Some(dir) = dir.filter(|_| drifted) {
                plans.push(SlotPlan::chain_only(idx, dir));
            }
        }
        for plan in &mut plans {
            plan.chain_nodes = chain::node_count(plan.dir.len());
        }
        self.place(plans)
    }

    /// Write already-serialised pages to **fresh tail blocks** and repoint
    /// their buckets — the defragmenter's move
    /// ([RFC 0042](../../../rfcs/0042-free-space-defragmentation.md)).
    ///
    /// Identical to a settle round except for where the window lands: best-fit
    /// would happily reuse the very hole the move is meant to enlarge.
    ///
    /// # Errors
    /// Propagates the window write fault; nothing is repointed if it fails.
    pub(crate) fn relocate(&self, plans: Vec<SlotPlan>) -> StorageResult<()> {
        self.place_in(plans, Placement::Tail)
    }

    /// Plan one round: one page image per touched bucket, per slot.
    fn plan_round(&self, round: &Touched) -> StorageResult<Vec<SlotPlan>> {
        let mut plans = Vec::with_capacity(round.len());
        for (idx, ids) in round {
            plans.push(self.plan_slot(*idx, ids)?);
        }
        Ok(plans)
    }

    /// Phases 2–4 + 6: carve one window for every planned image, write it,
    /// swap the descriptors in, and release the superseded runs.
    // The allocator guard deliberately spans carve → write → free: the window
    // must not be handed out twice, and a superseded run must not return to
    // the pool before the page replacing it is on disk.
    #[allow(clippy::significant_drop_tightening)]
    fn place(&self, plans: Vec<SlotPlan>) -> StorageResult<()> {
        self.place_in(plans, Placement::BestFit)
    }

    /// [`place`](Self::place) with the window's allocation strategy chosen.
    #[allow(clippy::significant_drop_tightening)]
    fn place_in(
        &self,
        mut plans: Vec<SlotPlan>,
        placement: Placement,
    ) -> StorageResult<()> {
        let targets = targets_of(&plans);
        if targets.is_empty() {
            return Ok(());
        }
        let mut alloc = self.alloc.lock();
        let sizes: Vec<u64> =
            targets.iter().map(|t| size_of(&plans, t)).collect();
        let total: u64 = sizes.iter().sum();

        // Nothing to place (every planned page emptied out) still repoints
        // descriptors — it just needs no window.
        let window = (total > 0).then(|| match placement {
            Placement::BestFit => alloc.alloc(total),
            Placement::Tail => alloc.alloc_tail(total),
        });
        let runs = window
            .map_or_else(|| vec![None; targets.len()], |w| carve(w, &sizes));
        // Pages take their addresses now, so the chain images built below
        // describe where everything actually landed.
        let freed = install(&mut plans, &targets, &runs);
        if let Some(window) = window {
            let buf = assemble(window, &plans, &targets, &runs);
            self.file.write_run(window, &buf)?; // the checkpoint's one write
        }
        for run in freed {
            alloc.free(run);
        }
        self.publish(plans, &targets, &runs, &mut alloc);
        Ok(())
    }

    /// Phase 4: adopt the working directories, the new dictionary runs and
    /// chain roots, and clear the tombstones the pages now satisfy.
    fn publish(
        &self,
        plans: Vec<SlotPlan>,
        targets: &[Target],
        runs: &[Option<Run>],
        alloc: &mut BlockAllocator,
    ) {
        for (plan, work) in plans.iter().enumerate() {
            let slot = self.types[work.idx];
            if let Some(run) = dict_run(targets, runs, plan) {
                let used = work.dict.as_ref().map_or(0, Vec::len) as u64;
                let old = slot
                    .dictionary()
                    .lock()
                    .repoint(BlockDescriptor::from_run_used(run, used));
                if old.is_allocated() {
                    alloc.free(old.run());
                }
            }
            let mut track = slot.chain().lock();
            if work.chain_nodes == 0 {
                track.dirty = true;
                continue;
            }
            let blocks: Vec<u64> = chain_runs(targets, runs, plan)
                .map(|run| run.start)
                .collect();
            for block in std::mem::take(&mut track.blocks) {
                alloc.free(Run::new(block, 1)); // copy-on-write: the old chain
            }
            track.root = blocks.first().copied().unwrap_or(0);
            track.blocks = blocks;
            track.dirty = false;
        }
        for work in plans {
            let slot = self.types[work.idx];
            for id in work.cleared {
                slot.clear_removed(id); // the page now agrees the id is gone
            }
            *slot.directory().lock() = Some(work.dir);
        }
    }
}

/// Everything this checkpoint places, in window order (a plan's chain nodes
/// stay contiguous, which is how their links are resolved).
fn targets_of(plans: &[SlotPlan]) -> Vec<Target> {
    let mut targets = Vec::new();
    for (plan, work) in plans.iter().enumerate() {
        for bucket in work.pages.keys() {
            targets.push(Target::Page {
                plan,
                bucket: *bucket,
            });
        }
        if work.dict.is_some() {
            targets.push(Target::Dict { plan });
        }
        for _ in 0..work.chain_nodes {
            targets.push(Target::Chain { plan });
        }
    }
    targets
}

/// Blocks a target occupies. A chain node is exactly one block; an emptied
/// page occupies none (its bucket becomes vacant).
fn size_of(plans: &[SlotPlan], target: &Target) -> u64 {
    match target {
        Target::Page { plan, bucket } => {
            plans[*plan].pages.get(bucket).map_or(0, |b| blocks_of(b))
        }
        Target::Dict { plan } => {
            plans[*plan].dict.as_deref().map_or(0, blocks_of)
        }
        Target::Chain { .. } => 1,
    }
}

/// Assign each target its run inside `window`, in order (`None` = no space).
fn carve(window: Run, sizes: &[u64]) -> Vec<Option<Run>> {
    let mut cursor = window.start;
    sizes
        .iter()
        .map(|&blocks| {
            (blocks > 0).then(|| {
                let run = Run::new(cursor, blocks);
                cursor += blocks;
                run
            })
        })
        .collect()
}

/// Point every planned bucket at the run it was just given, returning the
/// runs those buckets superseded.
fn install(
    plans: &mut [SlotPlan],
    targets: &[Target],
    runs: &[Option<Run>],
) -> Vec<Run> {
    let mut freed = Vec::new();
    for (target, run) in targets.iter().zip(runs) {
        let Target::Page { plan, bucket } = target else {
            continue;
        };
        let used = plans[*plan].pages.get(bucket).map_or(0, Vec::len) as u64;
        let old = plans[*plan].dir.descriptor(*bucket);
        if old.is_allocated() {
            freed.push(old.run());
        }
        let desc = run.map_or(BlockDescriptor::EMPTY, |run| {
            BlockDescriptor::from_run_used(run, used)
        });
        plans[*plan].dir.set_descriptor(*bucket, desc);
    }
    freed
}

/// Fill the window: every image at its carved offset, zero-padded between.
fn assemble(
    window: Run,
    plans: &[SlotPlan],
    targets: &[Target],
    runs: &[Option<Run>],
) -> Vec<u8> {
    let mut buf = vec![0u8; window.byte_len() as usize];
    let chains = chain_images(plans, targets, runs);
    for ((target, run), image) in targets.iter().zip(runs).zip(&chains) {
        let Some(run) = run else { continue };
        let bytes: &[u8] = match target {
            Target::Page { plan, bucket } => {
                plans[*plan].pages[bucket].as_ref()
            }
            Target::Dict { plan } => {
                plans[*plan].dict.as_deref().unwrap_or(&[])
            }
            Target::Chain { .. } => image.as_ref(),
        };
        let at = ((run.start - window.start) * BLOCK_SIZE as u64) as usize;
        buf[at..at + bytes.len()].copy_from_slice(bytes);
    }
    buf
}

/// Chain node images, indexed like `targets` (empty for every non-chain
/// target). Built after the carve so the links name real blocks and the
/// addresses are the ones the pages just took.
fn chain_images(
    plans: &[SlotPlan],
    targets: &[Target],
    runs: &[Option<Run>],
) -> Vec<Vec<u8>> {
    let mut images = vec![Vec::new(); targets.len()];
    for (plan, work) in plans.iter().enumerate() {
        if work.chain_nodes == 0 {
            continue;
        }
        let slots: Vec<usize> = chain_slots(targets, plan).collect();
        let blocks: Vec<u64> = slots
            .iter()
            .filter_map(|&i| runs[i].map(|run| run.start))
            .collect();
        let addresses: Vec<u64> =
            work.dir.slots().iter().map(|d| d.raw()).collect();
        for (&slot, image) in
            slots.iter().zip(chain::node_images(&addresses, &blocks))
        {
            images[slot] = image;
        }
    }
    images
}

/// Indices in `targets` holding `plan`'s chain nodes, in link order.
fn chain_slots(targets: &[Target], plan: usize) -> impl Iterator<Item = usize> {
    targets
        .iter()
        .enumerate()
        .filter(
            move |(_, t)| matches!(t, Target::Chain { plan: p } if *p == plan),
        )
        .map(|(i, _)| i)
}

/// The runs `plan`'s chain nodes were placed in.
fn chain_runs<'a>(
    targets: &'a [Target],
    runs: &'a [Option<Run>],
    plan: usize,
) -> impl Iterator<Item = Run> + 'a {
    chain_slots(targets, plan).filter_map(|i| runs[i])
}

/// The run `plan`'s dictionary image was placed in, if it grew.
fn dict_run(
    targets: &[Target],
    runs: &[Option<Run>],
    plan: usize,
) -> Option<Run> {
    targets
        .iter()
        .zip(runs)
        .find(|(t, _)| matches!(t, Target::Dict { plan: p } if *p == plan))
        .and_then(|(_, run)| *run)
}
