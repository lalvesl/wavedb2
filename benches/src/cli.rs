//! Command line of the benchmark runner.
//!
//! Split from the runner itself because the argument surface grew with the
//! server bracket: five seed paths, a system filter, and the guards.

use std::path::PathBuf;

pub struct Options {
    pub rows: u64,
    pub reads: u64,
    pub updates: u64,
    pub seed: u64,
    pub quick: bool,
    pub force: bool,
    pub keep: bool,
    pub repo: PathBuf,
    /// Prefilled seeds (Nix store paths). When present the insert phase is
    /// skipped, because a seeded run has nothing left to insert.
    pub seed_wavedb: Option<PathBuf>,
    pub seed_sqlite: Option<PathBuf>,
    pub seed_postgres: Option<PathBuf>,
    pub seed_mysql: Option<PathBuf>,
    pub seed_mongodb: Option<PathBuf>,
    /// Which workload(s) to run: `micro`, `shop` or `both`.
    pub workload: String,
    /// E-commerce workload sizes (RFC 0060 §3.1).
    pub users: u64,
    pub orders_max: u64,
    pub items_max: u64,
    pub signups: u64,
    pub checkouts: u64,
    pub profile_reads: u64,
    pub page_reads: u64,
    pub detail_reads: u64,
    /// Systems to run; empty means all of them.
    pub only: Vec<String>,
}

impl Options {
    #[must_use]
    pub fn wants(&self, system: &str) -> bool {
        self.only.is_empty() || self.only.iter().any(|s| s == system)
    }
}

pub const USAGE: &str = "\
usage: wavedb-bench [--rows N] [--reads N] [--updates N] [--seed N]
                    [--only a,b] [--quick] [--force] [--repo DIR]

  --quick   tiny sizes, prints but records nothing
  --force   record even when the machine is busy
  --keep    leave the scratch data directories behind for inspection
  --only    comma-separated systems to run, of wavedb, sqlite, mongodb,
            postgres, mysql (default: all of them)
  --workload micro | shop | both (default both)
            micro = one operation on one flat type; shop = the e-commerce
            workload, reported as latency of a composed operation

  shop sizes: --users --orders-max --items-max --signups --checkouts
              --profile-reads --page-reads
              --detail-reads

  --seed-<system> DIR
            use a prefilled seed (see `nix build .#bench-seed-*`); the insert
            phase is skipped, since a seeded run has nothing left to insert.
            Also read from BENCH_SEED_WAVEDB / _SQLITE / _POSTGRES / _MYSQL /
            _MONGODB.
  --no-seeds
            ignore those variables and fill from scratch";

fn path<'a>(
    it: &mut impl Iterator<Item = &'a String>,
    arg: &str,
) -> Result<PathBuf, String> {
    it.next()
        .ok_or_else(|| format!("{arg} needs a path"))
        .map(Into::into)
}

impl Options {
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut o = Self {
            rows: 100_000,
            reads: 50_000,
            updates: 50_000,
            seed: 42,
            quick: false,
            force: false,
            keep: false,
            repo: std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from(".")),
            // The flake app passes these; the environment keeps store paths
            // out of the command line the user types.
            seed_wavedb: std::env::var_os("BENCH_SEED_WAVEDB").map(Into::into),
            seed_sqlite: std::env::var_os("BENCH_SEED_SQLITE").map(Into::into),
            seed_postgres: std::env::var_os("BENCH_SEED_POSTGRES")
                .map(Into::into),
            seed_mysql: std::env::var_os("BENCH_SEED_MYSQL").map(Into::into),
            seed_mongodb: std::env::var_os("BENCH_SEED_MONGODB")
                .map(Into::into),
            workload: "both".into(),
            users: 20_000,
            orders_max: 20,
            items_max: 5,
            signups: 100,
            checkouts: 200,
            profile_reads: 10_000,
            page_reads: 1000,
            detail_reads: 1000,
            only: Vec::new(),
        };
        let mut it = args.iter();
        while let Some(arg) = it.next() {
            let mut value = || {
                it.next()
                    .ok_or_else(|| format!("{arg} needs a value"))?
                    .parse::<u64>()
                    .map_err(|e| format!("{arg}: {e}"))
            };
            match arg.as_str() {
                "--rows" => o.rows = value()?,
                "--reads" => o.reads = value()?,
                "--updates" => o.updates = value()?,
                "--seed" => o.seed = value()?,
                "--quick" => o.quick = true,
                "--force" => o.force = true,
                "--keep" => o.keep = true,
                "--repo" => {
                    o.repo = it
                        .next()
                        .ok_or("--repo needs a path")?
                        .parse()
                        .map_err(|e| format!("--repo: {e}"))?;
                }
                "--only" => {
                    o.only = it
                        .next()
                        .ok_or("--only needs a list")?
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                "--workload" => {
                    o.workload = it
                        .next()
                        .ok_or("--workload needs micro|shop|both")?
                        .clone();
                    if !["micro", "shop", "both"].contains(&o.workload.as_str())
                    {
                        return Err(format!(
                            "--workload {}: expected micro, shop or both",
                            o.workload
                        ));
                    }
                }
                "--users" => o.users = value()?,
                "--orders-max" => o.orders_max = value()?,
                "--items-max" => o.items_max = value()?,
                "--signups" => o.signups = value()?,
                "--checkouts" => o.checkouts = value()?,
                "--profile-reads" => o.profile_reads = value()?,
                "--page-reads" => o.page_reads = value()?,
                "--detail-reads" => o.detail_reads = value()?,
                "--seed-wavedb" => o.seed_wavedb = Some(path(&mut it, arg)?),
                "--seed-sqlite" => o.seed_sqlite = Some(path(&mut it, arg)?),
                "--seed-postgres" => {
                    o.seed_postgres = Some(path(&mut it, arg)?);
                }
                "--seed-mysql" => o.seed_mysql = Some(path(&mut it, arg)?),
                "--seed-mongodb" => o.seed_mongodb = Some(path(&mut it, arg)?),
                "--no-seeds" => {
                    o.seed_wavedb = None;
                    o.seed_sqlite = None;
                    o.seed_postgres = None;
                    o.seed_mysql = None;
                    o.seed_mongodb = None;
                }
                "-h" | "--help" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        if o.quick {
            o.users = o.users.min(40);
            o.orders_max = o.orders_max.min(12);
            o.signups = o.signups.min(20);
            o.checkouts = o.checkouts.min(20);
            o.profile_reads = o.profile_reads.min(200);
            o.page_reads = o.page_reads.min(100);
            o.detail_reads = o.detail_reads.min(100);
            o.rows = o.rows.min(2_000);
            o.reads = o.reads.min(2_000);
            o.updates = o.updates.min(2_000);
        }
        Ok(o)
    }
}
