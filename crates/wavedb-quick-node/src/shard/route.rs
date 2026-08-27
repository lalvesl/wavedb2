//! Which shard owns a collection ([RFC 0064]).
//!
//! A pure function of the Pivot's id, and that is the whole routing layer:
//! no table to consult, no split to schedule, nothing to migrate, and no
//! balancer. Uniformity comes from there being millions of Pivots — one per
//! holder, not one per type — rather than from anyone measuring load.
//!
//! [RFC 0064]: ../../../../rfcs/0064-pivot-owned-concurrency-PLANNED.md

use wavedb_core::LocalId;

/// The shard owning `pivot`, out of `shards`.
///
/// Stable for the life of the process: `shards` is fixed at startup and this
/// is a pure function, so a Pivot never changes owner and no ownership ever
/// migrates between threads. The price is that changing the shard count means
/// a restart.
#[must_use]
pub fn shard_of(pivot: LocalId, shards: usize) -> usize {
    if shards <= 1 {
        return 0;
    }
    // Both conversions are total rather than checked-and-hoped: `shards` came
    // from a `usize`, and the remainder is `< shards`, so it fits the type it
    // came from. The fallbacks are unreachable and say 0 rather than panic —
    // a routing function is on every request's path and has no business
    // being the thing that brings a node down.
    let Ok(n) = u64::try_from(shards) else {
        return 0;
    };
    usize::try_from(mix(bits(pivot)) % n).unwrap_or(0)
}

/// A `LocalId`'s bits as one integer. It has no raw accessor — `key` is the
/// instant, `flag`/`salt` are the packed low half — so they are recombined
/// here rather than exposed.
fn bits(pivot: LocalId) -> u64 {
    let lower = (u16::from(pivot.flag()) << 15) | pivot.salt();
    pivot.key() ^ (u64::from(lower) << 48)
}

/// SplitMix64's finalizer.
///
/// The mixing is not decoration. A Pivot's `key` is minted from
/// `platform::time::key_nanos()`, so ids created together are numerically
/// adjacent and share their high bits; `% shards` over raw instants would
/// deal collections out in creation order, which is a *pattern* rather than a
/// distribution — one burst of Pivots created in a loop would land in lockstep
/// with the shard count. Avalanche first, and the remainder is uniform for any
/// `shards`.
const fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::shard_of;
    use wavedb_core::LocalId;

    fn pivot(key: u64) -> LocalId {
        LocalId::new(key, false, 0x1234)
    }

    #[test]
    fn one_shard_owns_everything() {
        for key in 0..64 {
            assert_eq!(shard_of(pivot(key), 1), 0);
            assert_eq!(shard_of(pivot(key), 0), 0, "zero must not divide");
        }
    }

    #[test]
    fn a_pivot_always_routes_to_the_same_shard() {
        let p = pivot(1_787_000_000_000_000_123);
        let first = shard_of(p, 8);
        for _ in 0..100 {
            assert_eq!(shard_of(p, 8), first);
        }
    }

    #[test]
    fn every_shard_is_in_range() {
        for shards in 1..=16 {
            for key in 0..500 {
                assert!(shard_of(pivot(key), shards) < shards);
            }
        }
    }

    /// Adjacent instants must scatter, not deal round-robin.
    ///
    /// This is what the mix buys, and the test would pass trivially without it
    /// for `shards = 8` (raw `% 8` over `0..n` is perfectly uniform) — so the
    /// assertion is on *adjacency*: consecutive keys landing on consecutive
    /// shards is the pattern to refuse.
    #[test]
    fn consecutive_pivots_do_not_walk_the_shards_in_order() {
        let walked = (0..64u64)
            .filter(|&k| {
                let here = shard_of(pivot(k), 8);
                let next = shard_of(pivot(k + 1), 8);
                next == (here + 1) % 8
            })
            .count();
        // Round-robin would score 64. Chance alone puts this near 64/8 = 8.
        assert!(walked < 24, "{walked}/64 consecutive pivots stepped by one");
    }

    /// Collections spread across shards rather than piling onto one.
    #[test]
    fn pivots_spread_over_the_shards() {
        let shards = 8;
        let mut counts = vec![0usize; shards];
        // Real minting is `key_nanos()`: milliseconds times 1e6 plus a
        // sub-millisecond counter, so this is the shape of an actual burst.
        for n in 0..8_000u64 {
            let key = 1_787_000_000_000_000_000 + n * 1_000_000 + (n % 97);
            counts[shard_of(pivot(key), shards)] += 1;
        }
        let (lo, hi) =
            (*counts.iter().min().unwrap(), *counts.iter().max().unwrap());
        // 8000 over 8 shards is 1000 each; ±20% is loose enough not to be
        // flaky and tight enough to catch a hash that does not mix.
        assert!(lo > 800 && hi < 1200, "{counts:?}");
    }
}
