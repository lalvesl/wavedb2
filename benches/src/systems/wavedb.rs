//! The WaveDB adapter — engine in-process, the embedded bracket.
//!
//! Three constraints from RFC 0060 §5 shape this file:
//!
//! - **One store per process.** Every open is scoped so the `EngineClaim`
//!   drops before the next one; the cold-read phase depends on it.
//! - **The typed path only.** Reads go through `CollectionHandle::get`, which
//!   routes by `STRUCT_HASH`. `Store::get` is an untyped fallback that probes
//!   every slot — a path no generated code takes.
//! - **Steady state.** The settle queue is drained and the journal checkpointed
//!   before the "settled" footprint, so the number is not just work postponed.

use std::path::Path;

use futures::executor::block_on;
use wavedb_core::{CollectionHandle, LocalHandle, U48};
use wavedb_storage::PageStore;

use super::{Cfg, Durability, SystemReport};
use crate::RELAXED_WINDOW;
use crate::footprint::{Footprint, Point};
use crate::metrics::{self, Phase};
use crate::schema::{Rng, Thing, ThingPivotId, logical_bytes, thing, thing_v2};

const TENANT: u32 = 1;

/// Journal bytes that trigger a maintenance pass — `quick-node`'s own default
/// (`Maintenance::checkpoint_after_bytes`).
///
/// Byte-driven for the reason that default is byte-driven: an **operation
/// count cannot bound a log whose per-operation size depends on the data**.
/// Here it emphatically does. A first attempt checkpointed every 5 000 inserts
/// and never fired once: under 5 000 inserts of this schema had already
/// written **649 MB** of journal — on the order of 130 KB each, against a
/// ~400-byte record — while `data.bin` was still 4 KB.
///
/// That ratio is a finding in its own right and is **not explained here**; it
/// wants its own measurement before anyone attributes it to a mechanism.
const CHECKPOINT_AFTER_BYTES: u64 = 64 << 20;

/// What the write cache is evicted **down to** — not to zero.
///
/// Evicting to zero drops the hot B+tree interior nodes along with everything
/// else, so the next write descends through the page store instead of RAM; in
/// the shop adapter that made a fill 6× slower. This has to sit far enough
/// under the cage to leave room for the process itself.
const CACHE_BUDGET_BYTES: usize = 96 << 20;

/// One maintenance pass: checkpoint (which settles the queue into pages and
/// retires the journal) and bring the write cache back under budget. Exactly
/// what `quick-node`'s maintenance loop does, on the benchmark's own schedule.
///
/// Called from **inside** a timed phase but **outside** `Latencies::time`, so
/// it changes no latency and no `ops_per_sec` — those sum the individually
/// timed operations. It does land in the phase's `bytes_written`, which spans
/// the whole closure, and that is right: those bytes are really written.
fn maintain(store: &PageStore) {
    if store.journal_len() <= CHECKPOINT_AFTER_BYTES {
        return;
    }
    store.commit_journal().expect("checkpoint");
    store.evict_settled(CACHE_BUDGET_BYTES);
}
/// Defrag budget: generous enough that one pass is the compaction, since the
/// point of the "compacted" footprint is the floor, not a partial move.
const DEFRAG_BUDGET_BLOCKS: u64 = 1 << 20;

pub fn run(cfg: &Cfg, d: Durability) -> Result<SystemReport, String> {
    // Per row: the two durabilities must not share a store, or the second
    // would inherit the first's pages and its `insert` phase would measure a
    // rewrite.
    let dir = cfg.work_dir.join(format!("wavedb-{}", d.name()));
    // A seed arrives as `<store>/data` plus its `ids.bin`/`pivot.bin` sidecar;
    // the sidecar is what makes it usable at all, since a NonUnique anchor id
    // is minted from the clock and cannot be recomputed.
    let mut materialise_ms = 0;
    let seeded = if let Some(src) = &cfg.seed_wavedb {
        materialise_ms =
            crate::seed::materialise(src, &dir)?.as_millis() as u64;
        Some(crate::seed::load_wavedb_sidecar(&dir)?)
    } else {
        std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir: {e}"))?;
        None
    };

    let (mut phases, ids, pivot) = match seeded {
        Some((ids, pivot)) => {
            let data = dir.join("data");
            (read_only(cfg, &data, d, pivot, &ids)?, ids, pivot)
        }
        None => fill_and_read(cfg, &dir.join("data"), d)?,
    };
    let (update, footprints) =
        update_and_measure(cfg, &dir.join("data"), d, &ids, pivot)?;
    phases.push(update);

    Ok(SystemReport {
        system: "wavedb",
        bracket: "embedded",
        workload: "micro",
        durability: d,
        version: env!("CARGO_PKG_VERSION").to_string(),
        settings: vec![
            ("mode".into(), "PageStore (in-process)".into()),
            (
                "barrier".into(),
                match d {
                    Durability::Durable => "fsync per apply batch".to_string(),
                    Durability::Relaxed => {
                        format!("one fsync per elapsed {RELAXED_WINDOW:?}")
                    }
                },
            ),
            ("block_size".into(), "4096".into()),
        ],
        compression: "zstd (per-type dictionaries)",
        retains_history: true,
        phases,
        footprints,
        live_records: cfg.rows,
        logical_bytes: logical_bytes(cfg.rows, cfg.seed),
        notes: notes(cfg.seed_wavedb.is_some()),
        seed_path: cfg.seed_wavedb.as_ref().map(|p| p.display().to_string()),
        materialise_ms,
    })
}

fn notes(seeded: bool) -> Vec<String> {
    let mut notes = vec![
        "Every collection op is one apply batch. The durable row takes one \
         fsync per batch; the relaxed row takes one per elapsed window \
         (RFC 0061), the counterpart of the others' relaxed knobs."
            .into(),
        "Retains every superseded version; the other systems retain none. Read \
         the update row beside the footprint, never alone."
            .into(),
        "Barrier count is not recorded: PageStore exposes no public IoCounts \
         accessor and RFC 0060 forbids changing a shipped crate."
            .into(),
    ];
    if seeded {
        notes.push(
            "Seeded run: no insert phase (the insert benchmark IS the fill), \
             and no read_hot phase — the per-type cache is a write cache that \
             reads never populate, so on a store nobody has just written to, \
             hot and cold are the same measurement. RFC 0044 is that gap."
                .into(),
        );
    }
    notes
}

/// A seeded store's read phase. Only `read_cold` exists here, and the reason is
/// itself a finding: a read that misses the cache does not populate it, so a
/// store opened over prefilled pages has no warm state to measure.
fn read_only(
    cfg: &Cfg,
    dir: &Path,
    d: Durability,
    pivot: ThingPivotId,
    ids: &[wavedb_core::Id],
) -> Result<Vec<Phase>, String> {
    let store = open(dir, d)?;
    let db = LocalHandle::new(&store, U48::from(TENANT));
    let col = Thing::collection(pivot);
    Ok(vec![read_phase("read_cold", cfg, &db, col, ids)])
}

/// Insert every row, then read back hot (engine caches warm) and cold (store
/// reopened, so the caches are empty and reads fall through to settled pages).
fn fill_and_read(
    cfg: &Cfg,
    dir: &Path,
    d: Durability,
) -> Result<(Vec<Phase>, Vec<wavedb_core::Id>, ThingPivotId), String> {
    let mut ids = Vec::with_capacity(cfg.rows as usize);
    let mut phases = Vec::new();

    let pivot = {
        let store = open(dir, d)?;
        let db = LocalHandle::new(&store, U48::from(TENANT));
        let pivot = block_on(Thing::create_pivot(&db))
            .map_err(|e| format!("create_pivot: {e}"))?;
        let col = Thing::collection(pivot);

        phases.push(metrics::phase(
            "insert",
            |lat| {
                for n in 0..cfg.rows {
                    let t = thing(n, cfg.seed);
                    let id = lat.time(|| {
                        block_on(col.insert(&db, &t)).expect("insert")
                    });
                    ids.push(id);
                    maintain(&store);
                }
            },
            cfg.rows as usize,
        ));

        // Quiesce before reading so the read phase measures the read path and
        // not a settle that happened to land inside it.
        store.drain().map_err(|e| format!("drain: {e}"))?;
        phases.push(read_phase("read_hot", cfg, &db, col, &ids));
        store
            .commit_journal()
            .map_err(|e| format!("checkpoint: {e}"))?;
        pivot
    };

    // Reopen: engine caches are empty, so this is a genuine read-through to
    // pages. The OS page cache is still warm — see the note in `run`.
    {
        let store = open(dir, d)?;
        let db = LocalHandle::new(&store, U48::from(TENANT));
        let col = Thing::collection(pivot);
        phases.push(read_phase("read_cold", cfg, &db, col, &ids));
    }

    Ok((phases, ids, pivot))
}

/// Whole-record saves at known anchors, then the three footprint points.
fn update_and_measure(
    cfg: &Cfg,
    dir: &Path,
    d: Durability,
    ids: &[wavedb_core::Id],
    pivot: ThingPivotId,
) -> Result<(Phase, Vec<(Point, Footprint)>), String> {
    let store = open(dir, d)?;
    let db = LocalHandle::new(&store, U48::from(TENANT));
    // The same pivot the fill used: a collection's chains hang off it, so a
    // fresh one would save into a different collection entirely.
    let col = Thing::collection(pivot);

    let mut rng = Rng::new(cfg.seed ^ 0x0DDB_A11B_EEF0_0D15);
    let update = metrics::phase(
        "update",
        |lat| {
            for _ in 0..cfg.updates {
                let n = rng.below(cfg.rows);
                let id = ids[n as usize];
                let t = thing_v2(n, cfg.seed);
                lat.time(|| block_on(col.save(&db, id, &t)).expect("save"));
                // A save archives the superseded version, so this loop grows
                // the cache and the journal faster than the fill does.
                maintain(&store);
            }
        },
        cfg.updates as usize,
    );

    let mut points = vec![(Point::Hot, measure(dir)?)];
    points.push((Point::Settled, quiesce(&store, dir)?));
    store
        .defragment(DEFRAG_BUDGET_BLOCKS)
        .map_err(|e| format!("defragment: {e}"))?;
    points.push((Point::Compacted, quiesce(&store, dir)?));

    Ok((update, points))
}

/// Drain the settle queue and checkpoint **until the footprint stops moving**.
///
/// One checkpoint is not quiescence here: journal retirement is generational
/// (RFC 0047), so the journal a checkpoint supersedes is deleted by the one
/// after it. Measuring after a single round therefore counts a whole retained
/// journal as though it were stored data — which is how a 1.4 MB database
/// first measured as 34 MB. Looping until stable is the honest reading of
/// "the system's own natural quiescence", and it costs a few barriers.
fn quiesce(store: &PageStore, dir: &Path) -> Result<Footprint, String> {
    const MAX_ROUNDS: usize = 6;
    // `u64::MAX` forces at least two rounds: the first checkpoint supersedes
    // the journal, the second is the one that may delete it, so a size that
    // merely failed to grow proves nothing yet.
    let mut last = u64::MAX;
    for _ in 0..MAX_ROUNDS {
        store.drain().map_err(|e| format!("drain: {e}"))?;
        store
            .commit_journal()
            .map_err(|e| format!("checkpoint: {e}"))?;
        let now = measure(dir)?;
        if now.allocated_bytes == last && !store.has_pending() {
            return Ok(now);
        }
        last = now.allocated_bytes;
    }
    measure(dir)
}

/// A retired journal is 29 bytes and nothing here is preallocated, so the log
/// column exists on this row only to keep the comparison symmetric: every
/// system's recovery area is separated from its data (RFC 0060 §4.1).
fn measure(dir: &Path) -> Result<Footprint, String> {
    Footprint::split(dir, is_log).map_err(io)
}

fn is_log(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("journal_"))
}

fn read_phase(
    name: &'static str,
    cfg: &Cfg,
    db: &LocalHandle<'_, PageStore>,
    col: CollectionHandle<Thing>,
    ids: &[wavedb_core::Id],
) -> Phase {
    let mut rng = Rng::new(cfg.seed ^ u64::from(name.len() as u32));
    metrics::phase(
        name,
        |lat| {
            for _ in 0..cfg.reads {
                let id = ids[rng.below(ids.len() as u64) as usize];
                let got = lat.time(|| block_on(col.get(db, id)).expect("get"));
                assert!(got.is_some(), "{name}: missing record");
            }
        },
        cfg.reads as usize,
    )
}

/// Opened at the row's own durability (RFC 0061). Unlike the shop workload
/// there is no untimed preload to exempt here — the fill **is** the `insert`
/// phase — so every open in this adapter takes the row's window.
fn open(dir: &Path, d: Durability) -> Result<PageStore, String> {
    PageStore::open_with(
        dir,
        &Thing::storage_entries(),
        wavedb_storage::StoreOptions {
            relax_window: match d {
                Durability::Durable => std::time::Duration::ZERO,
                Durability::Relaxed => crate::RELAXED_WINDOW,
            },
        },
    )
    .map_err(|e| format!("open: {e}"))
}

fn io(e: std::io::Error) -> String {
    format!("footprint: {e}")
}
