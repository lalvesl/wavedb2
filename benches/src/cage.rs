//! The run's memory cage — and the one place it is deliberately opened.
//!
//! The 500 MB budget (RFC 0060 §5) exists to make the **measurement** honest:
//! it bounds the page cache, so a cold read is a disk read and a dataset
//! larger than memory is larger than memory. None of that reasoning applies to
//! a **fill**. A preload is scaffolding — untimed, never recorded — and
//! squeezing it only buys a slower build, so the fill gets the machine.
//!
//! `systemd-run --user --scope` delegates the scope's cgroup directory to us,
//! so the run can write its own `memory.max`. The scope is therefore created
//! **loose** (the fill's ceiling) and [`init`] tightens it to the measurement
//! budget immediately, which makes the tight cage the default state and the
//! loose one an explicit, scoped exception ([`for_fill`]).
//!
//! Everything here is a no-op outside the flake's cage — no `BENCH_MEM_MAX`,
//! no cgroup v2, or a file we may not write — so `cargo run` still works and
//! simply measures whatever the machine is.

use std::path::PathBuf;

/// Set by `nix run .#bench` to the **measurement** budget, in bytes.
const BUDGET_ENV: &str = "BENCH_MEM_MAX";

/// Tighten the cgroup to the measurement budget.
///
/// Call once, before anything is probed: [`crate::host`] reads `memory.max` to
/// key the lane, and the lane must be named for the budget the *numbers* ran
/// under, not the one the scope happened to be created with.
pub fn init() {
    tighten();
}

/// Set to a `memory.max` value to let fills run outside the measurement
/// budget. **Unset means fills stay caged**, which is the measured-faster
/// default — see the note on [`for_fill`].
const FILL_ENV: &str = "BENCH_FILL_MEM";

/// Open the cage for an untimed fill. Tightens again when the guard drops.
///
/// Off unless `BENCH_FILL_MEM` says otherwise, because giving the fill the
/// machine measured **worse**, not better: 8 000 shop users took 27 s filled
/// inside the 500 MB budget and 483 s filled at 10 GB, on an idle machine.
/// The fill is write-bound, not memory-bound — extra RAM does not make it need
/// fewer bytes on disk, it only lets dirty pages pile up until the tighten
/// forces the kernel to write them all back at once. Caged, the same writeback
/// happens incrementally and never becomes a cliff.
#[must_use]
pub fn for_fill() -> Loose {
    if let (Some(path), Ok(loose)) = (memory_max(), std::env::var(FILL_ENV)) {
        let _ = std::fs::write(path, loose.trim());
    }
    Loose
}

/// Restores the measurement budget on drop — so a `?` out of a preload cannot
/// leave the next system measuring inside a cage that was left open.
pub struct Loose;

impl Drop for Loose {
    fn drop(&mut self) {
        tighten();
    }
}

fn tighten() {
    let (Some(path), Ok(budget)) = (memory_max(), std::env::var(BUDGET_ENV))
    else {
        return;
    };
    // Lowering below current usage makes the kernel reclaim rather than fail,
    // which is exactly the intent: the page cache the fill warmed is what has
    // to go before a read can be called cold.
    let _ = std::fs::write(path, budget.trim());
}

/// This process's cgroup v2 `memory.max`, or `None` on v1 / no cgroup.
fn memory_max() -> Option<PathBuf> {
    let rel = std::fs::read_to_string("/proc/self/cgroup")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("0::").map(str::to_string))?;
    Some(PathBuf::from(format!("/sys/fs/cgroup{rel}/memory.max")))
}
