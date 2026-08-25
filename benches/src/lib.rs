//! WaveDB comparative benchmark — RFC 0060.
//!
//! Two binaries share this library: `wavedb-bench` measures, and `bench-gen`
//! fills. The split exists because the fill has to run **inside a Nix builder**
//! to become a cached seed derivation (§6), where nothing is timed and nothing
//! is recorded.

// Bench-scale arithmetic: row counts, byte totals and nanosecond sums are all
// far inside the ranges these lints guard, and the alternative — `try_from`
// plumbing on every count — would bury the measurement logic.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    // The workspace's own stance (root `Cargo.toml` lints): product names in
    // prose are not code spans.
    clippy::doc_markdown,
    // The error mappers exist to be passed as `map_err(sql)`, which needs the
    // by-value signature the lint objects to.
    clippy::needless_pass_by_value
)]

pub mod cli;
pub mod footprint;
pub mod host;
pub mod json;
pub mod metrics;
pub mod report;
pub mod schema;
pub mod seed;
pub mod shop;
pub mod tables;
pub mod systems;
