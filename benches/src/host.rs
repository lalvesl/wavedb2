//! The host fingerprint and the **host key** derived from it.
//!
//! Performance numbers are not portable across machines, so every recorded run
//! carries the machine it ran on and the corpus is read in lanes: two rows are
//! comparable only when their host keys match (RFC 0060 §7).

use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::json::fnv1a;

pub struct Host {
    pub cpu: String,
    /// CPUs the machine has.
    pub cores: u64,
    /// CPUs this run may actually use — `Cpus_allowed_list`, so a `taskset`
    /// cage shows up. `/proc/cpuinfo` would keep reporting all of them.
    pub cpu_budget: u64,
    /// RAM the machine has.
    pub mem_bytes: u64,
    /// RAM this run may actually use — the cgroup's `memory.max`, falling back
    /// to physical when unlimited. This bounds the **page cache** too, which is
    /// what makes a working set genuinely exceed memory (RFC 0060 §5).
    pub mem_budget: u64,
    pub kernel: String,
    pub filesystem: String,
    pub rotational: Option<bool>,
    pub virtualised: bool,
    /// `<cpu-slug>-<budget>c-<budget>g-<fs>-<hash4>` — the lane identity. The
    /// budgets are in it on purpose: a caged run and an uncaged one on the same
    /// machine are different lanes, because they are different machines as far
    /// as any number here is concerned.
    pub key: String,
}

impl Host {
    /// Fingerprint the machine, describing the filesystem of `data_dir`
    /// specifically — the disk under the benchmark is the one that matters,
    /// not the one under `/`.
    pub fn probe(data_dir: &Path) -> Self {
        let cpu = cpu_model();
        let cores = num_cpus();
        let cpu_budget = allowed_cpus().unwrap_or(cores);
        let mem_bytes = mem_total();
        let mem_budget = cgroup_memory_max().unwrap_or(mem_bytes).min(mem_bytes);
        let kernel = read_trim("/proc/sys/kernel/osrelease");
        let filesystem = fs_type(data_dir);
        let rotational = rotational(data_dir);
        let virtualised = cpu_flag("hypervisor");
        let mut key = format!(
            "{}-{cpu_budget}c-{}g-{filesystem}",
            slug(&cpu),
            mem_budget / (1 << 30)
        );
        let fingerprint = format!(
            "{cpu}|{cores}|{cpu_budget}|{mem_bytes}|{mem_budget}|{kernel}|\
             {filesystem}|{rotational:?}|{virtualised}"
        );
        let _ = write!(key, "-{:04x}", fnv1a(fingerprint.as_bytes()) & 0xffff);
        Self {
            cpu,
            cores,
            cpu_budget,
            mem_bytes,
            mem_budget,
            kernel,
            filesystem,
            rotational,
            virtualised,
            key,
        }
    }
}

/// 1-minute load average — the noise guard's input. A slow row recorded on a
/// busy machine is worse than a missing one: a future bisect blames a commit.
#[must_use]
pub fn load_average() -> f64 {
    read_trim("/proc/loadavg")
        .split_whitespace()
        .next()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

fn cpu_model() -> String {
    for line in read_trim("/proc/cpuinfo").lines() {
        if let Some(v) = line.strip_prefix("model name") {
            // `model name\t: Intel(R) …` — the tab is part of the separator.
            return v.trim_start_matches([' ', '\t', ':']).trim().to_string();
        }
    }
    "unknown-cpu".into()
}

fn cpu_flag(flag: &str) -> bool {
    read_trim("/proc/cpuinfo")
        .lines()
        .find(|l| l.starts_with("flags"))
        .is_some_and(|l| l.split_whitespace().any(|f| f == flag))
}

fn num_cpus() -> u64 {
    read_trim("/proc/cpuinfo")
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count() as u64
}

/// CPUs in this process's affinity mask, parsed from `Cpus_allowed_list`
/// (`0-3`, `0,2,4`, …). This is what `taskset` moves and `/proc/cpuinfo` does
/// not.
fn allowed_cpus() -> Option<u64> {
    let list = read_trim("/proc/self/status")
        .lines()
        .find_map(|l| l.strip_prefix("Cpus_allowed_list:").map(str::to_string))?;
    let mut n = 0;
    for part in list.trim().split(',') {
        match part.split_once('-') {
            Some((lo, hi)) => {
                let (lo, hi): (u64, u64) = (lo.parse().ok()?, hi.parse().ok()?);
                n += hi.saturating_sub(lo) + 1;
            }
            None if !part.is_empty() => n += 1,
            None => {}
        }
    }
    (n > 0).then_some(n)
}

/// This process's cgroup v2 `memory.max`, or `None` when unlimited or v1.
fn cgroup_memory_max() -> Option<u64> {
    // `0::/user.slice/…` — the unified hierarchy's line has an empty
    // controller field.
    let rel = read_trim("/proc/self/cgroup")
        .lines()
        .find_map(|l| l.strip_prefix("0::").map(str::to_string))?;
    let raw = read_trim(&format!("/sys/fs/cgroup{rel}/memory.max"));
    raw.trim().parse().ok()
}

fn mem_total() -> u64 {
    for line in read_trim("/proc/meminfo").lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            let kb: u64 =
                v.trim().trim_end_matches(" kB").parse().unwrap_or_default();
            return kb * 1024;
        }
    }
    0
}

/// The mount whose path is the longest prefix of `dir` owns it.
fn fs_type(dir: &Path) -> String {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let mounts = read_trim("/proc/mounts");
    let mut best = ("", "unknown");
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (Some(_dev), Some(point), Some(kind)) =
            (f.next(), f.next(), f.next())
        else {
            continue;
        };
        if dir.starts_with(point) && point.len() >= best.0.len() {
            best = (point, kind);
        }
    }
    best.1.to_string()
}

/// `/sys/dev/block/<major>:<minor>/queue/rotational`, falling back to the
/// parent device when `dir` sits on a partition.
fn rotational(dir: &Path) -> Option<bool> {
    let meta = std::fs::metadata(dir).ok()?;
    let dev = meta.dev();
    let (major, minor) = (libc_major(dev), libc_minor(dev));
    let base = format!("/sys/dev/block/{major}:{minor}");
    for path in [
        format!("{base}/queue/rotational"),
        format!("{base}/../queue/rotational"),
    ] {
        if let Ok(v) = std::fs::read_to_string(&path) {
            return Some(v.trim() == "1");
        }
    }
    None
}

// glibc's major/minor are macros; the encoding is stable enough to inline
// rather than take a `libc` dependency for two shifts.
const fn libc_major(dev: u64) -> u64 {
    ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)
}

const fn libc_minor(dev: u64) -> u64 {
    (dev & 0xff) | ((dev >> 12) & !0xff)
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars().take(48) {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

fn read_trim(path: &str) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim_end().to_string())
        .unwrap_or_default()
}
