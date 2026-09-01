//! `index.md` — one section per recorded run, rendered as markdown tables.
//!
//! The index is the human view of the corpus; the JSON record beside it is the
//! machine one. It used to be one prose line per run, which was compact and
//! unreadable: the numbers only compare column-wise, and a sentence cannot be
//! read column-wise. So a run now appends the same table it printed to the
//! terminal.
//!
//! **Append-only** (RFC 0060 §7): a section is written once and never revised,
//! and this module offers no way to rewrite one. That is also why the file
//! carries both formats — the prose lines above the first table are older runs,
//! left as they were recorded.
//!
//! Rows are comparable **only within one host key**, and the key is in every
//! section heading for that reason. Two tables from different lanes sitting one
//! above the other in one file are still two different machines.

use std::fmt::Write as _;

use crate::footprint::Point;
use crate::host::Host;
use crate::report::{Provenance, Skipped};
use crate::systems::SystemReport;
use crate::systems::shop::PHASES;

/// The whole section for one run, ready to append.
#[must_use]
pub fn section(
    host: &Host,
    prov: &Provenance,
    reports: &[SystemReport],
    skipped: &[Skipped],
) -> String {
    let mut out = format!(
        "## `{}` · `{}`{}\n\n{} · {} cpus · {} MB · load {:.2} at start\n",
        prov.timestamp,
        prov.git_sha,
        markers(prov),
        host.key,
        host.cpu_budget,
        host.mem_budget / (1 << 20),
        prov.load_average,
    );
    let micro: Vec<&SystemReport> =
        reports.iter().filter(|r| r.workload == "micro").collect();
    let shop: Vec<&SystemReport> =
        reports.iter().filter(|r| r.workload == "shop").collect();
    if !micro.is_empty() {
        let _ = write!(out, "\n{}", micro_table(&micro));
    }
    if !shop.is_empty() {
        let _ = write!(out, "\n{}", shop_table(&shop));
    }
    // Never omitted: a table that quietly lacks a system reads as a complete
    // run against a shorter field.
    for s in skipped {
        let _ = writeln!(out, "\nSKIPPED **{}** — {}", s.name, s.reason);
    }
    out.push('\n');
    out
}

/// What a reader has to know before comparing this section with another.
///
/// `uncaged` and `forced` can only appear together — the guard refuses to
/// record outside the cage unless overridden — but they say different things,
/// so both are printed.
fn markers(prov: &Provenance) -> String {
    let mut out = String::new();
    for (flag, text) in [
        (prov.dirty, "dirty tree"),
        (!prov.caged, "UNCAGED"),
        (prov.forced, "forced"),
    ] {
        if flag {
            let _ = write!(out, " **({text})**");
        }
    }
    out
}

/// Throughput per phase, plus what the dataset cost on disk.
///
/// The per-operation write columns of the terminal table (`kB/insert`,
/// `kB/update`) are left out here on purpose: they are a diagnostic for one
/// row, read while looking at that row, and in a nine-column markdown table
/// they push out the columns runs are actually compared on.
fn micro_table(reports: &[&SystemReport]) -> String {
    let mut out = String::from(
        "| system/row | bracket | insert/s | read_hot/s | read_cold/s | \
         update/s | payload | log | amp |\n|---|---|--:|--:|--:|--:|--:|--:|\
         --:|\n",
    );
    for r in reports {
        let settled = r.footprint(Point::Settled);
        let rate = |phase| r.phase(phase).map_or(0.0, |p| p.dist.ops_per_sec());
        let _ = writeln!(
            out,
            "| `{}` | {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.1}M | {:.1}M | \
             {:.2}× |",
            r.label(),
            r.bracket,
            rate("insert"),
            rate("read_hot"),
            rate("read_cold"),
            rate("update"),
            settled.payload_bytes() as f64 / 1e6,
            settled.log_bytes as f64 / 1e6,
            settled.amplification(r.logical_bytes),
        );
    }
    out
}

/// The e-commerce rows: **latency**, never a rate.
///
/// These phases are composed operations a customer waits on, so what matters is
/// what the slow tail costs — which a rate hides by construction. Reported the
/// same way the terminal table reports it, p50 beside p99.
fn shop_table(reports: &[&SystemReport]) -> String {
    let mut out = String::from(
        "e-commerce — median / p99 ms per operation\n\n\
         | system/row | bracket |",
    );
    for p in PHASES {
        let _ = write!(out, " {p} |");
    }
    out.push_str("\n|---|---|");
    for _ in PHASES {
        out.push_str("--:|");
    }
    out.push('\n');
    for r in reports {
        let _ = write!(out, "| `{}` | {} |", r.label(), r.bracket);
        for name in PHASES {
            match r.phase(name) {
                Some(p) => {
                    let ms = |ns: u64| ns as f64 / 1e6;
                    let _ = write!(
                        out,
                        " {:.2} / {:.2} |",
                        ms(p.dist.p50_ns),
                        ms(p.dist.p99_ns)
                    );
                }
                None => out.push_str(" — |"),
            }
        }
        out.push('\n');
    }
    out
}
