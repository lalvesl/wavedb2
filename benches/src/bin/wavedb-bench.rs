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
use wavedb_bench::systems::shop::ShopCfg;
use wavedb_bench::systems::{Cfg, Durability, SystemReport};
use wavedb_bench::tables::{print_shop_table, print_table};
use wavedb_bench::{host, report, systems};

/// Above this 1-minute load average the machine is too busy to record on. A
/// slow row that a future bisect blames on a commit is worse than no row.
const NOISE_LIMIT: f64 = 1.0;

/// Above this share of used data block groups, btrfs stops finding contiguous
/// free extents and starts carving new chunks, and a write-heavy row measures
/// the allocator rather than the database. Measured on one machine at 96%: the
/// same 8 000-user fill took **59 s and 1 297 s**, nothing else changed.
///
/// The same reasoning as [`NOISE_LIMIT`], applied to the other shared
/// resource. `df` will not warn you — it reports the device's free space,
/// while this is the fill of the groups already allocated.
const FILL_LIMIT: f64 = 0.90;

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
    let work_dir = std::env::temp_dir()
        .join(format!("wavedb-bench-{}", std::process::id()));
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
        match (host.data_fill, host.filesystem.as_str()) {
            (Some(f), fs) => format!("{fs} {:.0}% full", f * 100.0),
            // Loud on purpose: on btrfs the fill guard is the one that matters,
            // and a probe that quietly returns nothing reads exactly like a
            // healthy disk.
            (None, "btrfs") => "btrfs (FILL UNREADABLE — guard blind)".into(),
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
    if let Some(fill) = host.data_fill
        && fill > FILL_LIMIT
        && !opts.force
        && !opts.quick
    {
        return Err(fill_refusal(fill));
    }

    let mut reports = Vec::new();
    if opts.workload != "shop" {
        run_micro(opts, &cfg, &mut reports)?;
    }
    let shop_cfg = shop_cfg(opts, &work_dir);
    let mut shop = Vec::new();
    if opts.workload != "micro" {
        run_shop(opts, &shop_cfg, &mut shop)?;
    }
    if reports.is_empty() && shop.is_empty() {
        return Err("--only matched no system".into());
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
    if prov.load_average > NOISE_LIMIT && !opts.force {
        return Err(format!(
            "load average {:.2} exceeds {NOISE_LIMIT:.2} — refusing to record \
             a number this machine cannot stand behind (--force to override)",
            prov.load_average
        ));
    }
    let results = opts.repo.join("benches/results");
    let path =
        report::write(&results, &cfg, &shop_cfg, &host, &prov, &reports)?;
    println!("\nrecorded {}", path.display());
    if prov.dirty {
        println!("  marked dirty: this run measured uncommitted code.");
    }
    Ok(())
}

fn fill_refusal(fill: f64) -> String {
    format!(
        "btrfs data block groups are {:.1}% full (limit {:.0}%) — refusing to \
         run: past this the allocator, not the database, sets the write times. \
         `df` will disagree; it reports device free space, not the fill of the \
         groups already allocated. Free space or point the run at another \
         filesystem (--force to override, --quick to smoke-test anyway)",
        fill * 100.0,
        FILL_LIMIT * 100.0
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
) -> Result<(), String> {
    if opts.wants("wavedb") {
        for d in [Durability::Durable, Durability::Relaxed] {
            take(
                out,
                &format!("wavedb/{}", d.name()),
                systems::wavedb::run(cfg, d),
            )?;
        }
    }
    if opts.wants("sqlite") {
        for d in [Durability::Durable, Durability::Relaxed] {
            take(
                out,
                &format!("sqlite/{}", d.name()),
                systems::sqlite::run(cfg, d),
            )?;
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
            take(out, &format!("{name}/{}", d.name()), run(cfg, d))?;
        }
    }
    Ok(())
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
) -> Result<(), String> {
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
                &format!("shop wavedb/{}", d.name()),
                systems::shop::wavedb::run(cfg, d),
            )?;
        }
    }
    if opts.wants("sqlite") {
        for d in [Durability::Durable, Durability::Relaxed] {
            take(
                out,
                &format!("shop sqlite/{}", d.name()),
                systems::shop::sqlite::run(cfg, d),
            )?;
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
            take(out, &format!("shop {name}/{}", d.name()), run(cfg, d))?;
        }
    }
    Ok(())
}

fn take(
    reports: &mut Vec<SystemReport>,
    name: &str,
    result: Result<SystemReport, String>,
) -> Result<(), String> {
    eprint!("  {name} ... ");
    let report = result.map_err(|e| format!("{name}: {e}"))?;
    eprintln!("done");
    reports.push(report);
    Ok(())
}
