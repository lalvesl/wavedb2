//! WaveDB comparative benchmark — RFC 0060.
//!
//! Measures insert / read / update against SQLite in-process (the embedded
//! bracket) and against MongoDB, PostgreSQL and MySQL over a local connection
//! (the server bracket), and records one append-only result into
//! `benches/results/`. Rows from different brackets are never comparable: the
//! server rows carry a round trip the embedded rows do not.
//!
//! ```sh
//! nix run .#bench                      # full pass, records
//! nix run .#bench -- --quick           # smoke, does not record
//! nix run .#bench -- --rows 1000000    # explicit size
//! nix run .#bench -- --only wavedb,mongodb
//! ```

#![allow(clippy::cast_precision_loss, clippy::doc_markdown)]

use std::process::ExitCode;

use wavedb_bench::cli::{Options, USAGE};
use wavedb_bench::report::Skipped;
use wavedb_bench::systems::shop::ShopCfg;
use wavedb_bench::systems::{Cfg, Durability, SystemReport};
use wavedb_bench::tables::{print_shop_table, print_table};
use wavedb_bench::{host, report, systems};

/// How busy the machine may be and still be worth recording on, as a share of
/// the CPUs **this run** may actually use. A slow row that a future bisect
/// blames on a commit is worse than no row — but so is a guard that refuses an
/// ordinary desktop.
///
/// Relative, not absolute. A flat 1.00 reads as "one core's worth of demand":
/// about an eighth of an 8-core machine, and a quarter of the standard 4-CPU
/// cage, which is far stricter than the contention it is guarding against.
/// Half the run's own budget puts the cage at 2.00 and an uncaged 8-core run
/// at 4.00.
///
/// It stays a heuristic either way: load average counts every runnable task on
/// the machine, while `taskset` confines the benchmark to `benchCpus`, so work
/// pinned to the other cores inflates this without contending for much beyond
/// memory bandwidth and the disk.
const NOISE_PER_CPU: f64 = 0.5;

/// Never stricter than this, so a one- or two-CPU cage does not become
/// unrunnable on a machine that is merely awake.
const NOISE_FLOOR: f64 = 2.0;

fn noise_limit(cpu_budget: u64) -> f64 {
    (cpu_budget as f64 * NOISE_PER_CPU).max(NOISE_FLOOR)
}

/// Below this share of **unallocated** device space, btrfs can no longer carve
/// a fresh chunk when existing block groups are awkward, and a write-heavy row
/// starts measuring the allocator rather than the database.
///
/// The same reasoning as [`NOISE_PER_CPU`], applied to the other shared
/// resource. The state that correlated with a 22× spread on one machine — the
/// same 8 000-user fill at **59 s and 1 297 s** — was 8.5% unallocated *and*
/// 96% data fill. Data fill alone is the wrong guard: a `btrfs balance` packs
/// the groups it keeps, so it *raises* fill while restoring the headroom that
/// actually matters. Recorded, not guarded.
const UNALLOCATED_LIMIT: f64 = 0.10;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match Options::parse(&args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    match run(&opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(opts: &Options) -> Result<(), String> {
    let work_dir = std::env::temp_dir().join(work_dir_name());
    // `create_dir_all` succeeds on an existing directory, and *that* is the
    // hazard: a store left there by an earlier run is opened, not ignored, so
    // the run replays someone else's journal and measures a database it did
    // not build. Refuse instead — a fresh working directory is a precondition
    // of the measurement, not a convenience.
    if work_dir.exists() {
        return Err(format!(
            "working directory {} already exists — refusing to run against \
             another run's leftovers. Remove it and retry",
            work_dir.display()
        ));
    }
    std::fs::create_dir_all(&work_dir).map_err(|e| format!("workdir: {e}"))?;

    let cfg = Cfg {
        rows: opts.rows,
        reads: opts.reads,
        updates: opts.updates,
        seed: opts.seed,
        work_dir: work_dir.clone(),
        seed_wavedb: opts.seed_wavedb.clone(),
        seed_sqlite: opts.seed_sqlite.clone(),
        seed_postgres: opts.seed_postgres.clone(),
        seed_mysql: opts.seed_mysql.clone(),
        seed_mongodb: opts.seed_mongodb.clone(),
    };

    // Before the probe: the scope is created loose so fills get the machine,
    // and the lane must be keyed by the budget the *numbers* ran under.
    wavedb_bench::cage::init();
    let host = host::Host::probe(&work_dir);
    let prov = report::Provenance::probe(&opts.repo);
    eprintln!(
        "host {} · {}/{} cpus · {} MiB cap · {} · {}",
        host.key,
        host.cpu_budget,
        host.cores,
        host.mem_budget / (1 << 20),
        match (host.btrfs, host.filesystem.as_str()) {
            (Some(s), fs) => format!(
                "{fs} {:.0}% unallocated, groups {:.0}% full",
                s.unallocated * 100.0,
                s.data_fill * 100.0
            ),
            // Loud on purpose: on btrfs the space guard is the one that
            // matters, and a probe that quietly returns nothing reads exactly
            // like a healthy disk.
            (None, "btrfs") => "btrfs (SPACE UNREADABLE — guard blind)".into(),
            (None, fs) => fs.to_string(),
        },
        if prov.dirty {
            "dirty tree"
        } else {
            &prov.git_sha
        }
    );
    eprintln!(
        "rows {} · reads {} · updates {} · seed {}\n",
        cfg.rows, cfg.reads, cfg.updates, cfg.seed
    );

    // Checked *before* the work, unlike the load guard: a benchmark only ever
    // writes, so this number cannot improve while one is running. Waiting for
    // the end would learn nothing and burn the whole pass first — at the
    // default sizes, on a filesystem in this state, that is days.
    if let Some(space) = host.btrfs
        && space.unallocated < UNALLOCATED_LIMIT
        && !opts.force
        && !opts.quick
    {
        return Err(space_refusal(space));
    }

    let mut reports = Vec::new();
    let mut skipped = Vec::new();
    if opts.workload != "shop" {
        run_micro(opts, &cfg, &mut reports, &mut skipped);
    }
    let shop_cfg = shop_cfg(opts, &work_dir);
    let mut shop = Vec::new();
    if opts.workload != "micro" {
        run_shop(opts, &shop_cfg, &mut shop, &mut skipped);
    }
    if reports.is_empty() && shop.is_empty() {
        // Every row failing is a broken environment, not a result — that one
        // still aborts, and says why for each.
        return Err(if skipped.is_empty() {
            "--only matched no system".into()
        } else {
            format!(
                "every system failed — {}",
                skipped
                    .iter()
                    .map(|s| format!("{}: {}", s.name, s.reason))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        });
    }

    println!();
    if !reports.is_empty() {
        print_table(&reports);
    }
    if !shop.is_empty() {
        println!();
        print_shop_table(&shop);
    }
    reports.extend(shop);
    if opts.keep {
        println!("\nkept {}", work_dir.display());
    } else {
        let _ = std::fs::remove_dir_all(&work_dir);
    }

    if opts.quick {
        println!("\n--quick: nothing recorded.");
        return Ok(());
    }
    let noise = noise_limit(host.cpu_budget);
    if prov.load_average > noise && !opts.force {
        return Err(format!(
            "load average {:.2} at start exceeds {noise:.2} ({} cpus × \
             {NOISE_PER_CPU}, floor {NOISE_FLOOR:.2}) — refusing to record a \
             number this machine cannot stand behind (--force to override)",
            prov.load_average, host.cpu_budget
        ));
    }
    let results = opts.repo.join("benches/results");
    let path = report::write(
        &results, &cfg, &shop_cfg, &host, &prov, &reports, &skipped,
    )?;
    println!("\nrecorded {}", path.display());
    if prov.dirty {
        println!("  marked dirty: this run measured uncommitted code.");
    }
    // Repeated after the tables, where it will actually be read: a skip is
    // easy to scroll past when it happened forty minutes ago.
    for s in &skipped {
        println!("  SKIPPED {} — {}", s.name, s.reason);
    }
    Ok(())
}

/// A working-directory name no other run will pick.
///
/// The pid alone is not enough, and the way it failed is worth keeping: the
/// cage runs the benchmark under `bwrap --unshare-pid`, where
/// `std::process::id()` is **always 2**, so every caged run named the same
/// directory. One aborted 100 000-row fill left a 2 GB journal there, and
/// every later run opened it and OOM-killed itself replaying batches it had
/// not written — read as a benchmark bug for three runs before the directory
/// was looked at.
fn work_dir_name() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("wavedb-bench-{}-{stamp}", std::process::id())
}

fn space_refusal(space: host::BtrfsSpace) -> String {
    format!(
        "only {:.1}% of the btrfs device is unallocated (limit {:.0}%), with \
         its data block groups {:.1}% full — refusing to run: with no room to \
         carve a fresh chunk, the allocator rather than the database sets the \
         write times. Free space, run `btrfs balance` to return packed groups \
         to the unallocated pool, or point the run at another filesystem \
         (--force to override, --quick to smoke-test anyway)",
        space.unallocated * 100.0,
        UNALLOCATED_LIMIT * 100.0,
        space.data_fill * 100.0
    )
}

/// The three server adapters share one signature, so the loop over them can be
/// a table of plain fn pointers instead of three copies of the same body.
#[cfg(feature = "servers")]
type ServerRun = fn(&Cfg, Durability) -> Result<SystemReport, String>;

#[cfg(feature = "servers")]
type ShopServerRun = fn(&ShopCfg, Durability) -> Result<SystemReport, String>;

/// The micro workload: one operation on one flat type, per system.
fn run_micro(
    opts: &Options,
    cfg: &Cfg,
    out: &mut Vec<SystemReport>,
    skipped: &mut Vec<Skipped>,
) {
    if opts.wants("wavedb") {
        for d in [Durability::Durable, Durability::Relaxed] {
            take(
                out,
                skipped,
                &format!("wavedb/{}", d.name()),
                systems::wavedb::run(cfg, d),
            );
        }
    }
    if opts.wants("sqlite") {
        for d in [Durability::Durable, Durability::Relaxed] {
            take(
                out,
                skipped,
                &format!("sqlite/{}", d.name()),
                systems::sqlite::run(cfg, d),
            );
        }
    }
    #[cfg(feature = "servers")]
    for (name, run) in [
        ("mongodb", systems::mongodb::run as ServerRun),
        ("postgres", systems::postgres::run),
        ("mysql", systems::mysql::run),
    ] {
        if !opts.wants(name) {
            continue;
        }
        for d in [Durability::Durable, Durability::Relaxed] {
            take(out, skipped, &format!("{name}/{}", d.name()), run(cfg, d));
        }
    }
}

/// Built once, outside [`run_shop`], because the recorded result needs it even
/// when `--workload micro` ran nothing: a corpus row that omits its sizes is a
/// number nobody can reproduce.
fn shop_cfg(opts: &Options, work_dir: &std::path::Path) -> ShopCfg {
    ShopCfg {
        users: opts.users,
        orders_max: opts.orders_max,
        items_max: opts.items_max,
        signups: opts.signups,
        checkouts: opts.checkouts,
        profile_reads: opts.profile_reads,
        page_reads: opts.page_reads,
        detail_reads: opts.detail_reads,
        seed: opts.seed,
        work_dir: work_dir.to_path_buf(),
    }
}

/// The e-commerce workload: composed operations, reported as latency.
fn run_shop(
    opts: &Options,
    cfg: &ShopCfg,
    out: &mut Vec<SystemReport>,
    skipped: &mut Vec<Skipped>,
) {
    eprintln!(
        "\nshop: {} users · {} signups · {} checkouts · {} profile / {} page / \
         {} detail reads\n",
        cfg.users,
        cfg.signups,
        cfg.checkouts,
        cfg.profile_reads,
        cfg.page_reads,
        cfg.detail_reads
    );
    if opts.wants("wavedb") {
        for d in [Durability::Durable, Durability::Relaxed] {
            take(
                out,
                skipped,
                &format!("shop wavedb/{}", d.name()),
                systems::shop::wavedb::run(cfg, d),
            );
        }
    }
    if opts.wants("sqlite") {
        for d in [Durability::Durable, Durability::Relaxed] {
            take(
                out,
                skipped,
                &format!("shop sqlite/{}", d.name()),
                systems::shop::sqlite::run(cfg, d),
            );
        }
    }
    #[cfg(feature = "servers")]
    for (name, run) in [
        ("mongodb", systems::shop::mongodb::run as ShopServerRun),
        ("postgres", systems::shop::postgres::run),
        ("mysql", systems::shop::mysql::run),
    ] {
        if !opts.wants(name) {
            continue;
        }
        for d in [Durability::Durable, Durability::Relaxed] {
            take(
                out,
                skipped,
                &format!("shop {name}/{}", d.name()),
                run(cfg, d),
            );
        }
    }
}

/// Run one system's row. A failure **skips that row and continues**.
///
/// It used to abort the whole pass, and the cost of that was not theoretical:
/// `mysqld` needing longer than its start-up timeout inside the cage threw
/// away eight finished rows and fifty minutes. One slow server is not a reason
/// to discard four systems' measurements.
///
/// The skip is loud on the terminal and recorded in the run's JSON, because
/// the failure mode this must not have is a corpus row that looks complete and
/// quietly lacks a system.
fn take(
    reports: &mut Vec<SystemReport>,
    skipped: &mut Vec<Skipped>,
    name: &str,
    result: Result<SystemReport, String>,
) {
    eprint!("  {name} ... ");
    match result {
        Ok(report) => {
            eprintln!("done");
            reports.push(report);
        }
        Err(reason) => {
            eprintln!("SKIPPED — {reason}");
            skipped.push(Skipped {
                name: name.to_string(),
                reason,
            });
        }
    }
}
