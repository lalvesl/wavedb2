//! WaveDB compile-time front door: `#[wavedb]`, `#[server]`, `declare_objects!`.
//!
//! Implementation is staged. This crate currently exports no macros — it exists
//! so dependents (`wavedb-core` and friends) resolve while the engine is built
//! bottom-up. See `crates/wavedb-macros/README.md` for the target surface.
