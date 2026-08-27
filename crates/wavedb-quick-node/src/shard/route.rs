//! Which shard owns an operation ([RFC 0064]).
//!
//! A pure function of what the operation belongs to — a Pivot, or a Unique
//! type under a tenant — and that is the whole routing layer: no table to
//! consult, no split to schedule, nothing to migrate, and no balancer.
//! Uniformity comes from there being millions of owners (one Pivot per
//! holder, not one per type) rather than from anyone measuring load.
//!
//! [RFC 0064]: ../../../../rfcs/0064-pivot-owned-concurrency-PLANNED.md

use wavedb_core::{LocalId, U48, type_salt};

/// What an operation belongs to.
///
/// **Routing is a property of the operation, not of each id it writes**, and
/// this type is where that is said. The distinction is not pedantry — routing
/// per id would be wrong, and a Unique save is the clearest proof:
///
/// ```text
/// let slot = archive_id(hash, shape, authored, tenant);   // KEY = the instant
/// writes.push(Write::Put(slot, …));                       // the superseded version
/// writes.push(Write::Put(plan.live_id, …));               // KEY = the STRUCT_HASH
/// ```
///
/// One atomic batch, two ids with nothing in common — `hash(id) % N` would
/// split it across two shards and there would be no owner to be atomic in.
/// The same holds for a collection: its B+tree nodes and chain segments carry
/// ids of their own that would hash elsewhere, and they belong to the Pivot's
/// owner because the *operation* does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// A NonUnique collection, named by its Pivot.
    Collection(LocalId),
    /// A Unique type under a tenant. It has no Pivot — the anchor is
    /// `KEY = STRUCT_HASH` under that tenant — so the pair is the identity.
    /// It has no tree and no chains either: an anchor plus derived archive
    /// slots is all a Unique type stores.
    ///
    /// The live record and its whole version history land on one shard, and
    /// **not because their ids agree** — they do not. The anchor is
    /// `Id::new(STRUCT_HASH, tenant, true, 0)`, salt **zero**, while an
    /// archive is `Id::new(instant, tenant, false, type_salt(hash))`. They
    /// converge because the owner is computed from the operation, so the
    /// anchor's salt is never consulted.
    Unique { tenant: U48, struct_hash: u64 },
}

impl Owner {
    /// The shard this operation belongs to, out of `shards`.
    #[must_use]
    pub fn shard(self, shards: usize) -> usize {
        match self {
            Self::Collection(pivot) => shard_of(pivot, shards),
            Self::Unique {
                tenant,
                struct_hash,
            } => scatter(unique_key(tenant, struct_hash), shards),
        }
    }
}

/// A Unique owner's routing key: the tenant and the type's 15-bit salt
/// **concatenated**, not combined.
///
/// Both components have to be in it. Either alone concentrates load on one
/// shard — the tenant alone puts every type of a heavy writer together, the
/// type alone puts every tenant's `User` together — and `User` is exactly the
/// type every tenant has.
///
/// Concatenation rather than xor, because 48 + 15 = 63 bits **fit**: the key
/// is then injective, and two distinct owners can only collide at the final
/// `% shards`, which nothing can avoid. `tenant ^ struct_hash` is not
/// injective, and its collisions are not scattered — for *any* two types
/// `h1`, `h2`, every tenant pair related by `t2 = t1 ^ h1 ^ h2` collides. That
/// is a systematic family, not an accident.
///
/// The 15-bit `type_salt` rather than the whole `STRUCT_HASH` is what makes
/// the fit possible (48 + 64 does not). The price is that two types sharing
/// their low 15 bits route together within a tenant — already a known
/// condition, which the exposure registry warns about at compile time.
fn unique_key(tenant: U48, struct_hash: u64) -> u64 {
    (tenant.get() << SALT_BITS) | u64::from(type_salt(struct_hash))
}

/// Width of the `Id` salt field, and of the type discriminator in it.
const SALT_BITS: u32 = 15;

/// The shard owning `pivot`, out of `shards`.
///
/// Stable for the life of the process: `shards` is fixed at startup and this
/// is a pure function, so a Pivot never changes owner and no ownership ever
/// migrates between threads. The price is that changing the shard count means
/// a restart.
#[must_use]
pub fn shard_of(pivot: LocalId, shards: usize) -> usize {
    scatter(bits(pivot), shards)
}

/// Avalanche `key`, then fold it onto `shards`.
fn scatter(key: u64, shards: usize) -> usize {
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
    usize::try_from(mix(key) % n).unwrap_or(0)
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
    use super::{Owner, shard_of};
    use wavedb_core::{LocalId, U48};

    fn pivot(key: u64) -> LocalId {
        LocalId::new(key, false, 0x1234)
    }

    fn unique(tenant: u32, struct_hash: u64) -> Owner {
        Owner::Unique {
            tenant: U48::from(tenant),
            struct_hash,
        }
    }

    #[test]
    fn a_unique_type_always_routes_to_the_same_shard() {
        let owner = unique(7, 0xDEAD_BEEF_CAFE_0001);
        let first = owner.shard(8);
        for _ in 0..100 {
            assert_eq!(owner.shard(8), first);
        }
        assert!(first < 8);
        assert_eq!(owner.shard(1), 0);
    }

    /// Both components must matter. A route that ignored either would put
    /// every tenant's `User` on one shard, or every type of one tenant there.
    #[test]
    fn tenant_and_type_both_move_the_route() {
        let by_tenant: std::collections::HashSet<usize> =
            (0..64u32).map(|t| unique(t, 0xAAAA).shard(8)).collect();
        assert!(by_tenant.len() > 4, "the tenant barely moved the route");

        let by_type: std::collections::HashSet<usize> = (0..64u64)
            .map(|h| unique(7, 0xAAAA_0000_0000_0000 ^ h).shard(8))
            .collect();
        assert!(by_type.len() > 4, "the type barely moved the route");
    }

    /// The routing key is **injective**: distinct owners never share one.
    ///
    /// This is what concatenation buys over combining, and the assertion an
    /// xor cannot pass. `tenant ^ struct_hash` collides systematically — for
    /// any two types `h1`, `h2`, every tenant pair with `t2 = t1 ^ h1 ^ h2`
    /// maps to the same key — so a whole family of owners would share a shard
    /// for a structural reason rather than by the pigeonhole every hash pays.
    #[test]
    fn distinct_unique_owners_never_share_a_routing_key() {
        let mut seen = std::collections::HashMap::new();
        for tenant in 0..64u64 {
            for salt in 0..64u64 {
                // Vary only the low 15 bits, which is what the key uses; the
                // rest of a STRUCT_HASH is deliberately not in it.
                let key = super::unique_key(U48::from_truncated(tenant), salt);
                assert!(
                    seen.insert(key, (tenant, salt)).is_none(),
                    "({tenant}, {salt}) collided with {:?}",
                    seen[&key]
                );
            }
        }
    }

    /// The concrete collision an xor would have. Under concatenation the two
    /// owners keep distinct keys.
    #[test]
    fn the_xor_collision_family_does_not_arise() {
        let (h1, h2) = (0x0000_0000_0000_1234u64, 0x0000_0000_0000_5678u64);
        let t1 = 0u64;
        let t2 = t1 ^ h1 ^ h2; // xor would map both to the same key
        assert_eq!(t1 ^ h1, t2 ^ h2, "the family this test is about");
        assert_ne!(
            super::unique_key(U48::from_truncated(t1), h1),
            super::unique_key(U48::from_truncated(t2), h2),
        );
    }

    #[test]
    fn unique_owners_spread_over_the_shards() {
        let shards = 8;
        let mut counts = vec![0usize; shards];
        for t in 0..4_000u32 {
            counts[unique(t, 0x1234_5678_9ABC_DEF0).shard(shards)] += 1;
        }
        let (lo, hi) =
            (*counts.iter().min().unwrap(), *counts.iter().max().unwrap());
        assert!(lo > 400 && hi < 600, "{counts:?}");
    }

    /// The two owner kinds share one keyspace of shards, so a collection and
    /// a Unique type can land together — which is fine, and worth pinning so
    /// nobody later assumes a shard is one kind or the other.
    #[test]
    fn both_owner_kinds_route_into_the_same_range() {
        for n in 0..64u64 {
            assert!(Owner::Collection(pivot(n)).shard(4) < 4);
            assert!(unique(u32::try_from(n).unwrap_or(0), n).shard(4) < 4);
        }
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
