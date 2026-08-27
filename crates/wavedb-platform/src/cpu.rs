//! CPU parallelism — how many threads this process may actually use.
//!
//! A **measurement, not a policy**. It answers "how much CPU is this process
//! allowed", and nothing here decides what to do with the answer;
//! [RFC 0064](../../../rfcs/0064-pivot-owned-concurrency-PLANNED.md) is what
//! says the shard count is this number.
//!
//! Native reads [`std::thread::available_parallelism`], **not** an online-CPU
//! count. The difference is the whole reason this module exists: a process
//! under an affinity mask or a container CPU quota is told the machine's core
//! count by the naive APIs, and would then start one shard per core it cannot
//! run on — every one of them competing for the slice it actually has, which is
//! worse than running the right number.
//!
//! Verified on this machine (8 cores) rather than assumed, because the two
//! restrictions are imposed by different mechanisms and a reader has no reason
//! to trust that both are honoured:
//!
//! | environment | `Cpus_allowed_list` | `cpu.max` | reported |
//! |---|---|---|---|
//! | unrestricted | `0-7` | `max` | 8 |
//! | `taskset -c 0,1` | `0-1` | `max` | 2 |
//! | `systemd-run -p CPUQuota=200%` | `0-7` | `200000 100000` | 2 |
//!
//! The third row is the load-bearing one: full affinity, a two-core quota, and
//! it still answered 2 — so the cgroup v2 quota is read, not just the mask.

/// Threads this process may usefully run, never zero.
///
/// Falls back to 1 when the platform cannot report — a wrong-but-safe answer
/// (one shard is always correct, merely slower) rather than a guess at the
/// hardware, which is what the fallback would otherwise be.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn available() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// Always **1**, and that is the true answer rather than a stubbed one.
///
/// The browser build has no threads to count: there is no tokio in wasm, the
/// engine's futures are non-`Send`, and `task::spawn_detached` is
/// `wasm_bindgen_futures::spawn_local` on the one thread that exists.
/// `navigator.hardwareConcurrency` would report the machine's cores and every
/// one of them would be unusable.
#[cfg(target_arch = "wasm32")]
#[must_use]
pub const fn available() -> usize {
    1
}

#[cfg(test)]
mod tests {
    use super::available;

    #[test]
    fn parallelism_is_never_zero() {
        // The only value that would break every caller: a shard count of zero
        // is not a degraded mode, it is a process that runs nothing.
        assert!(available() >= 1);
    }

    /// It never exceeds the affinity mask this process is confined to.
    ///
    /// This is the regression a naive replacement would cause — an online-CPU
    /// count (`nproc`-style, `_SC_NPROCESSORS_ONLN`) reports the machine and
    /// ignores the mask, so it over-reports the moment CI, a container, or a
    /// `taskset` narrows it. Reading `/proc/self/status` is the independent
    /// source: it is where the kernel publishes the mask, and it is not what
    /// the implementation consults.
    ///
    /// Honest limit: on an **unconstrained** machine this passes trivially
    /// (`8 <= 8`), so it only bites where the mask is narrowed — CI containers,
    /// the benchmark cage, `taskset`. Checked by replacing the body with a
    /// hard-coded 8 and running under `taskset -c 0,1`: *"reported 8 usable
    /// CPUs but the affinity mask allows 2"*.
    #[cfg(target_os = "linux")]
    #[test]
    fn parallelism_never_exceeds_the_affinity_mask() {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return; // no procfs — nothing to check against
        };
        let Some(list) = status
            .lines()
            .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
        else {
            return;
        };
        // `0-3,8,12-13` — ranges and singletons, comma-separated.
        let allowed: usize = list
            .trim()
            .split(',')
            .filter_map(|part| match part.split_once('-') {
                Some((lo, hi)) => {
                    let (lo, hi) =
                        (lo.parse::<usize>().ok()?, hi.parse::<usize>().ok()?);
                    Some(hi.checked_sub(lo)? + 1)
                }
                None => part.parse::<usize>().ok().map(|_| 1),
            })
            .sum();
        assert!(
            allowed > 0 && available() <= allowed,
            "reported {} usable CPUs but the affinity mask allows {allowed} ({list})",
            available(),
        );
    }
}
