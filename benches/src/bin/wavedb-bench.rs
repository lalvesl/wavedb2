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

use wavedb_bench::footprint::Point;
use wavedb_bench::systems::{Cfg, Durability, SystemReport};
use wavedb_bench::cli::{Options, USAGE};
use wavedb_bench::systems::shop::{PHASES, ShopCfg};
use wavedb_bench::{host, report, systems};

/// Above this 1-minute load average the machine is too busy to record on. A
/// slow row that a future bisect blames on a commit is worse than no row.
const NOISE_LIMIT: f64 = 1.0;

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

    let host = host::Host::probe(&work_dir);
    let prov = report::Provenance::probe(&opts.repo);
    eprintln!(
        "host {} · {} cores · {} · {}",
        host.key,
        host.cores,
        host.filesystem,
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

    let mut reports = Vec::new();
    if opts.workload != "shop" {
        run_micro(opts, &cfg, &mut reports)?;
    }
    let mut shop = Vec::new();
    if opts.workload != "micro" {
        run_shop(opts, &work_dir, &mut shop)?;
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
    let path = report::write(&results, &cfg, &host, &prov, &reports)?;
    println!("\nrecorded {}", path.display());
    if prov.dirty {
        println!("  marked dirty: this run measured uncommitted code.");
    }
    Ok(())
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
        take(out, "wavedb", systems::wavedb::run(cfg))?;
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

/// The e-commerce workload: composed operations, reported as latency.
fn run_shop(
    opts: &Options,
    work_dir: &std::path::Path,
    out: &mut Vec<SystemReport>,
) -> Result<(), String> {
    let cfg = ShopCfg {
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
    };
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
        take(out, "shop wavedb", systems::shop::wavedb::run(&cfg))?;
    }
    if opts.wants("sqlite") {
        for d in [Durability::Durable, Durability::Relaxed] {
            take(
                out,
                &format!("shop sqlite/{}", d.name()),
                systems::shop::sqlite::run(&cfg, d),
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
            take(out, &format!("shop {name}/{}", d.name()), run(&cfg, d))?;
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

/// The e-commerce table. **Latency, not throughput**: these phases are composed
/// operations a customer waits on, and the number that matters is what the slow
/// tail costs, which a rate hides by construction. p99 is printed beside p50 for
/// exactly that reason.
fn print_shop_table(reports: &[SystemReport]) {
    println!("e-commerce workload — median / p99 milliseconds per operation");
    print!("{:<18} {:>8}", "system/row", "bracket");
    for p in PHASES {
        print!(" {p:>21}");
    }
    println!(" {:>11} {:>8}", "kB/checkout", "payload");
    for r in reports {
        print!("{:<18} {:>8}", r.label(), r.bracket);
        for name in PHASES {
            match r.phase(name) {
                Some(p) => print!(
                    " {:>10.2}/{:<10.2}",
                    ms(p.dist.p50_ns),
                    ms(p.dist.p99_ns)
                ),
                None => print!(" {:>21}", "—"),
            }
        }
        println!(
            " {:>11.1} {:>7.1}M",
            kb_per_op(r, "checkout"),
            r.footprint(Point::Settled).payload_bytes() as f64 / 1e6
        );
    }
    println!(
        "\ncheckout is one order plus its line items. The other four commit it \
         as one\ntransaction — one barrier; WaveDB has no multi-record \
         transaction, so it is one\nbatch and one barrier per record. \
         order_page is ten orders: a declared list's page\ndescent on WaveDB, \
         ORDER BY … LIMIT 10 OFFSET n everywhere else."
    );
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

/// Kilobytes actually sent to the block layer per operation of `phase`
/// (`/proc/<pid>/io`'s `write_bytes`, so page-cache-absorbed writes do not
/// count). Beside a record of a few hundred bytes this is the write
/// amplification, and it is what a disk pinned at 100% while the CPU idles
/// looks like from the outside.
fn kb_per_op(r: &SystemReport, phase: &str) -> f64 {
    r.phase(phase).map_or(0.0, |p| {
        if p.dist.count == 0 {
            return 0.0;
        }
        p.bytes_written as f64 / p.dist.count as f64 / 1024.0
    })
}

fn print_table(reports: &[SystemReport]) {
    println!(
        "{:<18} {:>8} {:>9} {:>10} {:>10} {:>8} {:>9} {:>9} {:>7} {:>6} {:>6}",
        "system/row",
        "bracket",
        "insert/s",
        "read_hot/s",
        "read_cold/s",
        "update/s",
        "kB/insert",
        "kB/update",
        "payload",
        "log",
        "amp"
    );
    for r in reports {
        let settled = r.footprint(Point::Settled);
        println!(
            "{:<18} {:>8} {:>9.0} {:>10.0} {:>10.0} {:>8.0} {:>9.1} {:>9.1} {:>6.1}M {:>5.1}M {:>5.2}×",
            r.label(),
            r.bracket,
            r.phase("insert").map_or(0.0, |p| p.dist.ops_per_sec()),
            r.phase("read_hot").map_or(0.0, |p| p.dist.ops_per_sec()),
            r.phase("read_cold").map_or(0.0, |p| p.dist.ops_per_sec()),
            r.phase("update").map_or(0.0, |p| p.dist.ops_per_sec()),
            kb_per_op(r, "insert"),
            kb_per_op(r, "update"),
            settled.payload_bytes() as f64 / 1e6,
            settled.log_bytes as f64 / 1e6,
            settled.amplification(r.logical_bytes),
        );
    }
    let baselines: Vec<String> = reports
        .iter()
        .filter(|r| r.footprint(Point::Baseline).allocated_bytes > 0)
        .map(|r| {
            let b = r.footprint(Point::Baseline);
            format!(
                "{} {:.1}M",
                r.label(),
                b.payload_bytes() as f64 / 1e6
            )
        })
        .collect();
    if !baselines.is_empty() {
        println!(
            "\nempty-system baseline (payload, before any of this dataset): {}",
            baselines.join(", ")
        );
    }
    println!(
        "\nwavedb retains every superseded version; nobody else does — the \
         update and space\ncolumns are one trade, not two results. `log` is \
         preallocated recovery capacity:\na configured constant, not a \
         function of the data. `amp` is payload only, baseline\nincluded — at \
         small row counts a server's amplification is mostly its own \
         furniture.\nEmbedded and server rows are different brackets: only \
         compare within one."
    );
}

