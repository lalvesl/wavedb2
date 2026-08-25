//! The two result tables.
//!
//! Separate files because they answer different questions and print different
//! units: the micro table is a rate, the shop table is a latency distribution.

use crate::footprint::Point;
use crate::systems::SystemReport;
use crate::systems::shop::PHASES;


/// The e-commerce table. **Latency, not throughput**: these phases are composed
/// operations a customer waits on, and the number that matters is what the slow
/// tail costs, which a rate hides by construction. p99 is printed beside p50 for
/// exactly that reason.
pub fn print_shop_table(reports: &[SystemReport]) {
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

pub fn print_table(reports: &[SystemReport]) {
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

