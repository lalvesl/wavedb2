//! Writing the results record, and appending to the corpus index.
//!
//! The corpus is **append-only** (RFC 0060 §7): a past run is never edited to
//! look better. JSON is the stored form because it diffs and machines read it;
//! `index.md` gets one human line per run so `git log` on that file reads as a
//! history.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::footprint::Point;
use crate::host::Host;
use crate::json::{Json, fnv1a};
use crate::systems::{Cfg, SystemReport};

pub struct Provenance {
    pub git_sha: String,
    pub dirty: bool,
    pub flake_lock: String,
    pub timestamp: String,
    pub load_average: f64,
}

impl Provenance {
    pub fn probe(repo: &Path) -> Self {
        let git_sha = git(repo, &["rev-parse", "--short=7", "HEAD"])
            .unwrap_or_else(|| "unknown".into());
        let dirty = git(repo, &["status", "--porcelain"])
            .is_some_and(|s| !s.trim().is_empty());
        let flake_lock = std::fs::read(repo.join("flake.lock")).map_or_else(
            |_| "absent".into(),
            |b| format!("{:016x}", fnv1a(&b)),
        );
        Self {
            git_sha,
            dirty,
            flake_lock,
            timestamp: utc_stamp(),
            load_average: crate::host::load_average(),
        }
    }
}

/// Write one run's record and index line. Returns the record's path.
pub fn write(
    results_dir: &Path,
    cfg: &Cfg,
    shop: &crate::systems::shop::ShopCfg,
    host: &Host,
    prov: &Provenance,
    reports: &[SystemReport],
) -> Result<PathBuf, String> {
    let lane = results_dir.join(&host.key);
    std::fs::create_dir_all(&lane).map_err(|e| format!("mkdir: {e}"))?;
    let name = format!("{}-{}.json", prov.timestamp, prov.git_sha);
    let path = lane.join(&name);

    std::fs::write(&path, record(cfg, shop, host, prov, reports))
        .map_err(|e| format!("write record: {e}"))?;
    append_index(results_dir, host, prov, reports)?;
    Ok(path)
}

fn record(
    cfg: &Cfg,
    shop: &crate::systems::shop::ShopCfg,
    host: &Host,
    prov: &Provenance,
    reports: &[SystemReport],
) -> String {
    let mut j = Json::new();
    j.obj(None, |j| {
        j.str("schema", "wavedb-bench/1");
        j.str("timestamp", &prov.timestamp);
        j.obj(Some("provenance"), |j| {
            j.str("git_sha", &prov.git_sha);
            j.boolean("dirty", prov.dirty);
            j.str("flake_lock_fnv1a", &prov.flake_lock);
            j.ratio("load_average_at_start", prov.load_average);
        });
        j.obj(Some("host"), |j| {
            j.str("key", &host.key);
            j.str("cpu", &host.cpu);
            j.num("cores", host.cores);
            j.num("cpu_budget", host.cpu_budget);
            j.num("mem_bytes", host.mem_bytes);
            j.num("mem_budget_bytes", host.mem_budget);
            j.str("kernel", &host.kernel);
            j.str("filesystem", &host.filesystem);
            match host.rotational {
                Some(r) => j.boolean("rotational", r),
                None => j.str("rotational", "unknown"),
            }
            j.boolean("virtualised", host.virtualised);
            // Recorded, never keyed: it moves between runs on one machine.
            // A row that looks slow is readable only beside this.
            match host.data_fill {
                Some(f) => j.ratio("data_block_group_fill", f),
                None => j.str("data_block_group_fill", "n/a (not btrfs)"),
            }
        });
        j.obj(Some("workload"), |j| {
            j.num("rows", cfg.rows);
            j.num("reads", cfg.reads);
            j.num("updates", cfg.updates);
            j.num("seed", cfg.seed);
            j.str("access", "uniform random over the key space");
            j.str("os_page_cache_on_cold_read", "warm");
        });
        // The shop sizes, recorded for the same reason `rows` is: a row that
        // does not say how much data it ran against is a number nobody can
        // reproduce or compare. Without this a `--users 8000` calibration and
        // a full `--users 200000` pass land in the corpus indistinguishable.
        j.obj(Some("shop_workload"), |j| {
            j.num("users", shop.users);
            j.num("orders_max", shop.orders_max);
            j.num("items_max", shop.items_max);
            j.num("live_records", shop.live_records());
            j.num("signups", shop.signups);
            j.num("checkouts", shop.checkouts);
            j.num("profile_reads", shop.profile_reads);
            j.num("page_reads", shop.page_reads);
            j.num("detail_reads", shop.detail_reads);
        });
        j.arr(Some("systems"), |j| {
            for r in reports {
                j.obj(None, |j| system(j, r));
            }
        });
        j.arr(Some("out_of_scope"), |j| {
            j.elem("joins and ad-hoc predicates (WaveDB has none)");
            j.elem("history comparison — RFC 0060 phase 4");
            j.elem("server bracket: MongoDB, PostgreSQL, MySQL — phases 2-3");
            j.elem("concurrency sweep — phase 5");
        });
    });
    j.finish()
}

fn system(j: &mut Json, r: &SystemReport) {
    j.str("system", r.system);
    j.str("bracket", r.bracket);
    j.str("durability_row", r.durability.name());
    j.str("version", &r.version);
    j.str("compression", r.compression);
    j.boolean("retains_history", r.retains_history);
    j.obj(Some("settings"), |j| {
        for (k, v) in &r.settings {
            j.str(k, v);
        }
    });
    j.arr(Some("phases"), |j| {
        for p in &r.phases {
            j.obj(None, |j| {
                j.str("name", p.name);
                j.num("count", p.dist.count);
                j.ratio("ops_per_sec", p.dist.ops_per_sec());
                j.num("p50_ns", p.dist.p50_ns);
                j.num("p95_ns", p.dist.p95_ns);
                j.num("p99_ns", p.dist.p99_ns);
                j.num("max_ns", p.dist.max_ns);
                j.num("disk_write_bytes", p.bytes_written);
            });
        }
    });
    j.arr(Some("footprint"), |j| {
        for (point, f) in &r.footprints {
            j.obj(None, |j| {
                j.str("point", point.name());
                j.num("apparent_bytes", f.apparent_bytes);
                j.num("allocated_bytes", f.allocated_bytes);
                j.num("files", f.files);
                j.ratio("bytes_per_record", f.bytes_per_record(r.live_records));
                j.ratio("amplification", f.amplification(r.logical_bytes));
            });
        }
    });
    j.num("live_records", r.live_records);
    j.num("logical_bytes", r.logical_bytes);
    match &r.seed_path {
        Some(p) => {
            j.str("seed", p);
            j.num("materialise_ms", r.materialise_ms);
        }
        None => j.str("seed", "none (filled by this run)"),
    }
    j.arr(Some("notes"), |j| {
        for n in &r.notes {
            j.elem(n);
        }
    });
}

fn append_index(
    results_dir: &Path,
    host: &Host,
    prov: &Provenance,
    reports: &[SystemReport],
) -> Result<(), String> {
    let index = results_dir.join("index.md");
    if !index.exists() {
        std::fs::write(&index, INDEX_HEADER)
            .map_err(|e| format!("index: {e}"))?;
    }
    let mut line = format!(
        "- `{}` · {} · `{}`{} — ",
        prov.timestamp,
        host.key,
        prov.git_sha,
        if prov.dirty { " **(dirty tree)**" } else { "" }
    );
    for (i, r) in reports.iter().enumerate() {
        if i > 0 {
            line.push_str("; ");
        }
        let settled = r.footprint(Point::Settled);
        let _ = write!(
            line,
            "{}: insert {:.0}/s, read_cold {:.0}/s, update {:.0}/s, {:.2}× space",
            r.label(),
            r.phase("insert").map_or(0.0, |p| p.dist.ops_per_sec()),
            r.phase("read_cold").map_or(0.0, |p| p.dist.ops_per_sec()),
            r.phase("update").map_or(0.0, |p| p.dist.ops_per_sec()),
            settled.amplification(r.logical_bytes),
        );
    }
    line.push('\n');

    let mut text =
        std::fs::read_to_string(&index).map_err(|e| format!("index: {e}"))?;
    text.push_str(&line);
    std::fs::write(&index, text).map_err(|e| format!("index: {e}"))
}

const INDEX_HEADER: &str = "\
# Benchmark results

Append-only (RFC 0060 §7). One line per recorded run; the JSON record beside it
carries the full configuration. **Rows are comparable only within one host
key** — a different machine is a different lane, never a trend line.

`update` rows: WaveDB retains every superseded version and the others retain
none, so read them beside the space column.

";

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `YYYY-MM-DDTHH-MMZ`, computed from the epoch so the record needs no clock
/// crate. Colons are avoided: the stamp is a filename.
fn utc_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hour, min) = (rem / 3600, (rem % 3600) / 60);
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}-{min:02}Z")
}

/// Howard Hinnant's `civil_from_days`, the standard days-to-date algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
