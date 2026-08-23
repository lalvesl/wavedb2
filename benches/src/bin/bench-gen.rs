//! The seed filler — runs inside a Nix builder, never inside a measurement.
//!
//! ```sh
//! bench-gen emit-tsv     --rows N --seed S --out dataset.tsv
//! bench-gen fill-wavedb  --rows N --seed S --out seed-dir
//! ```
//!
//! `emit-tsv` is the portable form: PostgreSQL, MySQL, MongoDB and SQLite seeds
//! are all built by handing that one file to each system's own bulk loader
//! (`COPY`, `LOAD DATA`, `mongoimport`, `.import`), which is why those seeds can
//! exist before their Rust adapters do. WaveDB gets its own mode because it has
//! no bulk path at all — one insert is one batch is one `fsync`, so a large
//! WaveDB seed genuinely takes a while to build. That is the same missing group
//! commit the benchmark exists to measure, met from the other side.

use std::path::PathBuf;
use std::process::ExitCode;

use wavedb_bench::seed;

const USAGE: &str = "\
usage: bench-gen <emit-tsv|fill-wavedb> --rows N --seed S --out PATH";

fn main() -> ExitCode {
    match run() {
        Ok(msg) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<String, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().ok_or("missing mode")?.clone();
    let mut rows = 0u64;
    let mut seed = 42u64;
    let mut out = PathBuf::new();

    let mut it = args[1..].iter();
    while let Some(arg) = it.next() {
        let mut next = || it.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--rows" => {
                rows = next()?.parse().map_err(|e| format!("--rows: {e}"))?;
            }
            "--seed" => {
                seed = next()?.parse().map_err(|e| format!("--seed: {e}"))?;
            }
            "--out" => out = PathBuf::from(next()?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if rows == 0 {
        return Err("--rows must be > 0".into());
    }
    if out.as_os_str().is_empty() {
        return Err("--out is required".into());
    }

    match mode.as_str() {
        "emit-tsv" => {
            seed::emit_tsv(&out, rows, seed)?;
            Ok(format!("wrote {rows} rows ({}) to {}", seed::COLUMNS, out.display()))
        }
        "fill-wavedb" => {
            seed::fill_wavedb(&out, rows, seed)?;
            Ok(format!("filled {rows} records into {}", out.display()))
        }
        other => Err(format!("unknown mode {other}")),
    }
}
