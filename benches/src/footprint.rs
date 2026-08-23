//! Storage footprint (RFC 0060 §4.1).
//!
//! Both numbers are taken because they answer different questions: apparent
//! size is what the data claims to occupy, allocated blocks are what the
//! filesystem actually spent. WaveDB allocates in 4 KiB runs and leaves free
//! runs behind, so the gap between the two *is* fragmentation.

use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Tells preallocated recovery capacity (WAL, redo, journal) from data.
///
/// A plain `fn` pointer, one per system, because the classification is a fact
/// about that server's file names and nothing else.
pub type IsLog = fn(&Path) -> bool;

/// Nothing in this directory is log capacity.
#[must_use]
pub fn no_logs(_: &Path) -> bool {
    false
}

/// One measurement point's footprint of a whole data directory.
#[derive(Debug, Clone, Copy, Default)]
pub struct Footprint {
    /// Sum of file sizes (`st_size`).
    pub apparent_bytes: u64,
    /// Sum of allocated blocks (`st_blocks` × 512) — sparse files show here.
    pub allocated_bytes: u64,
    /// Of `allocated_bytes`, the part that is preallocated recovery capacity.
    /// It is a **configured constant**, the same size at 200 000 rows and at
    /// 20, so it is reported apart from the data or the headline stops being a
    /// comparison of databases and becomes one of default log settings
    /// (RFC 0060 §4.1).
    pub log_bytes: u64,
    pub files: u64,
}

impl Footprint {
    /// Walk `dir` recursively, counting everything the system needs to serve
    /// this data after a restart, and splitting off what `is_log` claims.
    pub fn split(dir: &Path, is_log: IsLog) -> std::io::Result<Self> {
        let mut out = Self::default();
        walk(dir, is_log, &mut out)?;
        Ok(out)
    }

    /// Walk `dir` with no log classification — everything counts as payload.
    pub fn of(dir: &Path) -> std::io::Result<Self> {
        Self::split(dir, no_logs)
    }

    /// Allocated bytes that are data rather than recovery capacity. This is the
    /// number a headline may quote.
    #[must_use]
    pub const fn payload_bytes(&self) -> u64 {
        self.allocated_bytes.saturating_sub(self.log_bytes)
    }

    /// Payload bytes per live record.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn bytes_per_record(&self, live: u64) -> f64 {
        if live == 0 {
            return 0.0;
        }
        self.payload_bytes() as f64 / live as f64
    }

    /// Payload bytes ÷ logical payload bytes — the cross-system ratio.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn amplification(&self, logical: u64) -> f64 {
        if logical == 0 {
            return 0.0;
        }
        self.payload_bytes() as f64 / logical as f64
    }
}

fn walk(dir: &Path, is_log: IsLog, out: &mut Footprint) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let path = entry.path();
        if meta.is_dir() {
            walk(&path, is_log, out)?;
        } else {
            let allocated = meta.blocks() * 512;
            out.files += 1;
            out.apparent_bytes += meta.size();
            out.allocated_bytes += allocated;
            if is_log(&path) {
                out.log_bytes += allocated;
            }
        }
    }
    Ok(())
}

/// The three points a footprint is taken at. A single number is meaningless
/// because every system defers different work, so all three are reported and
/// never conflated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Point {
    /// The system initialised and running with **none** of this dataset in it.
    ///
    /// Not a comparison row — a correction term. An empty PostgreSQL cluster is
    /// tens of megabytes of system catalogs and an empty MySQL more, so at
    /// small row counts a server's amplification is mostly its own furniture.
    /// Recording it lets a reader see that instead of inferring it.
    Baseline,
    /// Right after the run, deferred work still pending.
    Hot,
    /// After the system's own natural quiescence — the fair comparison.
    Settled,
    /// After explicitly asking for the smallest form.
    Compacted,
}

impl Point {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Hot => "hot",
            Self::Settled => "settled",
            Self::Compacted => "compacted",
        }
    }
}
