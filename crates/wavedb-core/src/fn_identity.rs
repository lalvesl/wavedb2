//! A `#[server]` function's composed identity.
//!
//! [`compose`] folds the function's name hash with each argument's and the
//! return's **type tag**, so evolving any argument type's schema
//! transitively renames the function (a mixed-build caller fails the header
//! gate instead of mis-decoding).
//!
//! Two pieces:
//!
//! - [`FnArgTag`] — the `const` 64-bit tag of a type as a function
//!   argument/return. `#[wavedb]` structs tag as their `STRUCT_HASH`
//!   (schema evolution propagates); builtins carry fixed tags; containers
//!   compose their element's.
//! - [`compose`] — a `const fn` mixer (SplitMix64 folds). **Not SeaHash**:
//!   the hash must be computable in `const` context from other crates'
//!   `STRUCT_HASH` consts, and the `seahash` crate is not `const`. The
//!   mixer is identity-load-bearing all the same — it is fixed arithmetic,
//!   portable by construction, and pre-release layout policy applies (a
//!   change renames every function, caught at the boundary, no migration).

use crate::id::Id;
use crate::local_id::LocalId;
use crate::metadata::Metadata;
use crate::permission::PermissionRef;
use crate::u48::U48;

/// Domain separator so a composed fn hash can't collide with a plain
/// name-string hash by construction.
const FN_DOMAIN: u64 = 0xF7A5_11D3_0C4B_9E21;

/// SplitMix64's finalizer — the standard 64-bit avalanche.
const fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fold `parts` (argument tags in order, then the return tag) into `seed`
/// (the function's name hash). Order-sensitive: swapping two argument types
/// yields a different identity.
#[must_use]
pub const fn compose(seed: u64, parts: &[u64]) -> u64 {
    let mut h = mix(seed ^ FN_DOMAIN);
    let mut i = 0;
    while i < parts.len() {
        // Position folds in via the running state, not an index term.
        h = mix(h ^ parts[i]);
        i += 1;
    }
    h
}

/// Tag a container's element under a kind marker (`Vec<T>` ≠ `Option<T>` of
/// the same `T`).
#[must_use]
pub const fn container(kind: u64, element: u64) -> u64 {
    mix(mix(kind ^ FN_DOMAIN) ^ element)
}

/// The `const` identity of a type in a `#[server]` signature.
///
/// `#[wavedb]` structs get `TAG = STRUCT_HASH` (macro-implemented), so a
/// schema change to an argument renames every function using it. Builtins
/// carry fixed tags; containers compose. A type without an impl cannot be a
/// server-function argument or return — a compile error, not a wire risk.
pub trait FnArgTag {
    /// This type's 64-bit signature tag.
    const TAG: u64;
}

/// Fixed tags for the wire builtins (arbitrary distinct constants — their
/// exact values only need to be stable and unequal).
macro_rules! fixed_tag {
    ($($ty:ty => $tag:expr),+ $(,)?) => {
        $(impl FnArgTag for $ty { const TAG: u64 = $tag; })+
    };
}

fixed_tag! {
    ()     => 0xB111_0000_0000_0001,
    bool   => 0xB111_0000_0000_0002,
    char   => 0xB111_0000_0000_0003,
    u8     => 0xB111_0000_0000_0010,
    u16    => 0xB111_0000_0000_0011,
    u32    => 0xB111_0000_0000_0012,
    u64    => 0xB111_0000_0000_0013,
    u128   => 0xB111_0000_0000_0014,
    i8     => 0xB111_0000_0000_0020,
    i16    => 0xB111_0000_0000_0021,
    i32    => 0xB111_0000_0000_0022,
    i64    => 0xB111_0000_0000_0023,
    i128   => 0xB111_0000_0000_0024,
    f32    => 0xB111_0000_0000_0030,
    f64    => 0xB111_0000_0000_0031,
    String => 0xB111_0000_0000_0040,
}

// WaveDB's own wire values are valid signature types with fixed tags.
fixed_tag! {
    Id            => 0xB111_0000_0000_0100,
    LocalId       => 0xB111_0000_0000_0101,
    U48           => 0xB111_0000_0000_0102,
    Metadata      => 0xB111_0000_0000_0103,
    PermissionRef => 0xB111_0000_0000_0104,
}

/// Container kind markers.
const VEC_KIND: u64 = 0xC017_0000_0000_0001;
const OPTION_KIND: u64 = 0xC017_0000_0000_0002;
const ARRAY_KIND: u64 = 0xC017_0000_0000_0003;
/// A stream-returning fn's return kind (`impl Stream<Item = Result<T>>`
/// composes as `container(STREAM_KIND, T::TAG)` — `#[server]` emits it).
pub const STREAM_KIND: u64 = 0xC017_0000_0000_0004;

impl<T: FnArgTag> FnArgTag for Vec<T> {
    const TAG: u64 = container(VEC_KIND, T::TAG);
}

impl<T: FnArgTag> FnArgTag for Option<T> {
    const TAG: u64 = container(OPTION_KIND, T::TAG);
}

impl<T: FnArgTag, const N: usize> FnArgTag for [T; N] {
    const TAG: u64 = compose(ARRAY_KIND, &[T::TAG, N as u64]);
}

/// Tuple tags compose their members in order under a length seed.
macro_rules! tuple_tag {
    ($len:expr => $($name:ident)+) => {
        impl<$($name: FnArgTag),+> FnArgTag for ($($name,)+) {
            const TAG: u64 =
                compose(0xD0_0F_0000_0000_0000 + $len, &[$($name::TAG),+]);
        }
    };
}

tuple_tag!(2 => A B);
tuple_tag!(3 => A B C);
tuple_tag!(4 => A B C D);

#[cfg(test)]
mod tests {
    use super::{FnArgTag, compose, container};

    #[test]
    fn compose_is_order_sensitive_and_deterministic() {
        let a = compose(1, &[10, 20]);
        assert_eq!(a, compose(1, &[10, 20]), "deterministic");
        assert_ne!(a, compose(1, &[20, 10]), "argument order matters");
        assert_ne!(a, compose(2, &[10, 20]), "the name seed matters");
        assert_ne!(a, compose(1, &[10]), "arity matters");
    }

    #[test]
    fn container_tags_separate_kinds_and_elements() {
        assert_ne!(<Vec<u64>>::TAG, <Option<u64>>::TAG);
        assert_ne!(<Vec<u64>>::TAG, <Vec<u32>>::TAG);
        assert_ne!(<Vec<Vec<u64>>>::TAG, <Vec<u64>>::TAG, "nesting folds in");
        assert_eq!(container(1, 2), container(1, 2));
    }

    #[test]
    fn tuples_and_arrays_fold_members() {
        assert_ne!(<(u64, String)>::TAG, <(String, u64)>::TAG);
        assert_ne!(<[u8; 4]>::TAG, <[u8; 5]>::TAG, "length folds in");
        assert_ne!(<(u64, u64)>::TAG, <(u64, u64, u64)>::TAG);
    }
}
