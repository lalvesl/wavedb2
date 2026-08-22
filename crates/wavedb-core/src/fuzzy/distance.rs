//! Verification and ranking: the exact answer the prefilter only approximated.
//!
//! Steps 3 and 4 of the read ([RFC 0056]) reject candidates cheaply and admit
//! false positives on purpose. This module is step 5 — the one that makes the
//! result **exact**, by computing the real distance (or the real Jaccard) on
//! the record's own bytes. Everything upstream exists to keep the set that
//! reaches here small, because each survivor costs a random read.
//!
//! [RFC 0056]: https://github.com/wavedb/wavedb/blob/main/rfcs/0056-fuzzy-string-search-WIP.md

use super::grams::GramSet;

/// One verified hit and what it scored.
///
/// `rank` is always "smaller is better", whichever mode produced it — a
/// distance is already that, and a similarity is stored as `1.0 - t`. One
/// ordering rule for two modes, so the sort has no mode to branch on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scored<T> {
    /// What was found.
    pub item: T,
    /// The mode's own score: edit distance, or Jaccard similarity.
    pub score: f64,
    /// Sort key, ascending — best first.
    pub rank: f64,
}

impl<T> Scored<T> {
    /// A hit at edit distance `d`.
    #[must_use]
    pub fn at_distance(item: T, d: usize) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let d = d as f64;
        Self {
            item,
            score: d,
            rank: d,
        }
    }

    /// A hit at Jaccard similarity `t`.
    #[must_use]
    pub fn at_similarity(item: T, t: f64) -> Self {
        Self {
            item,
            score: t,
            rank: 1.0 - t,
        }
    }
}

/// Exact Jaccard similarity of two gram sets: `|a ∩ b| / |a ∪ b|`.
///
/// Two empty sets are perfectly similar (`1.0`) rather than undefined — an
/// empty query matching an empty field is a match, and it is the answer that
/// keeps the caller from having to special-case it.
#[must_use]
pub fn jaccard(a: &GramSet, b: &GramSet) -> f64 {
    let shared = a.intersection(b).count();
    let union = a.len() + b.len() - shared;
    if union == 0 {
        return 1.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let (shared, union) = (shared as f64, union as f64);
    shared / union
}

/// Trigram **containment** of `query` in `record`: `|q ∩ r| / |q|`.
///
/// Asymmetric on purpose — it asks "how much of what was typed is in this
/// record?", so a perfect substring is `1.0` however long the record is. That
/// independence from the record's length is the whole difference from
/// [`jaccard`], and it is what makes this the type-ahead measure.
///
/// An empty query contains trivially (`1.0`), matching [`jaccard`]'s answer
/// for two empty sets rather than inventing a second convention.
#[must_use]
pub fn containment(query: &GramSet, record: &GramSet) -> f64 {
    if query.is_empty() {
        return 1.0;
    }
    let shared = query.intersection(record).count();
    #[allow(clippy::cast_precision_loss)]
    let (shared, total) = (shared as f64, query.len() as f64);
    shared / total
}

/// Levenshtein distance between `a` and `b`, or `None` when it exceeds `k`.
///
/// Banded: only the `2k+1` diagonals around the main one can hold a value
/// within `k`, so the work is `O(k · min(|a|,|b|))` rather than the full
/// product. The bound is the point — a verify step that cost a full DP per
/// survivor would undo what the filters bought.
#[must_use]
pub fn levenshtein_within(a: &[char], b: &[char], k: usize) -> Option<usize> {
    // The trivial rejection, and the one that makes the band well-defined.
    if a.len().abs_diff(b.len()) > k {
        return None;
    }
    // Iterate with the shorter string as the row so the row is min-length.
    let (a, b) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let width = a.len() + 1;

    let mut prev: Vec<usize> = (0..width).collect();
    let mut cur = vec![0usize; width];
    for (j, cb) in b.iter().enumerate() {
        cur[0] = j + 1;
        // The band: outside it every cell already exceeds `k`, so computing
        // it would only produce a value we are about to discard.
        let lo = (j + 1).saturating_sub(k);
        let hi = (j + 1 + k).min(a.len());
        let mut best = usize::MAX;
        for i in 1..width {
            cur[i] = if i < lo || i > hi {
                // Poison rather than a real cost: `saturating_add` below keeps
                // it from wrapping back into a plausible number.
                usize::MAX
            } else {
                let sub =
                    prev[i - 1].saturating_add(usize::from(a[i - 1] != *cb));
                let del = prev[i].saturating_add(1);
                let ins = cur[i - 1].saturating_add(1);
                sub.min(del).min(ins)
            };
            best = best.min(cur[i]);
        }
        // Every cell in the row is already over budget, so no completion of
        // it can come back under.
        if best > k && cur[0] > k {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let d = prev[a.len()];
    (d <= k).then_some(d)
}

#[cfg(test)]
mod tests {
    use super::{Scored, jaccard, levenshtein_within};
    use crate::fuzzy::fold::{Fold, normalize};
    use crate::fuzzy::grams::{DEFAULT_N, gram_prefixes};

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    /// The reference implementation the banded one must agree with: the full
    /// DP, no band, no early exit.
    fn brute(a: &[char], b: &[char]) -> usize {
        let mut prev: Vec<usize> = (0..=a.len()).collect();
        for (j, cb) in b.iter().enumerate() {
            let mut cur = vec![j + 1; a.len() + 1];
            for i in 1..=a.len() {
                cur[i] = (prev[i - 1] + usize::from(a[i - 1] != *cb))
                    .min(prev[i] + 1)
                    .min(cur[i - 1] + 1);
            }
            prev = cur;
        }
        prev[a.len()]
    }

    #[test]
    fn known_distances() {
        assert_eq!(
            levenshtein_within(&chars("kitten"), &chars("sitting"), 3),
            Some(3)
        );
        assert_eq!(
            levenshtein_within(&chars("kitten"), &chars("sitting"), 2),
            None
        );
        assert_eq!(
            levenshtein_within(&chars("ada"), &chars("ada"), 0),
            Some(0)
        );
        assert_eq!(levenshtein_within(&chars(""), &chars("abc"), 3), Some(3));
        assert_eq!(levenshtein_within(&chars(""), &chars(""), 0), Some(0));
    }

    // The banded DP is an optimisation of the full one, so the only thing
    // that makes it trustworthy is agreeing with the full one everywhere.
    // A band that is too narrow does not fail — it silently reports "no
    // match" for a pair that matches, which is the exact failure mode RFC
    // 0056 says nothing downstream would notice.
    #[test]
    fn banded_agrees_with_brute_force_over_generated_pairs() {
        let alphabet = ['a', 'b', 'c'];
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..4000 {
            let la = (next() % 9) as usize;
            let lb = (next() % 9) as usize;
            let a: Vec<char> =
                (0..la).map(|_| alphabet[(next() % 3) as usize]).collect();
            let b: Vec<char> =
                (0..lb).map(|_| alphabet[(next() % 3) as usize]).collect();
            let truth = brute(&a, &b);
            for k in 0..=6 {
                let got = levenshtein_within(&a, &b, k);
                let want = (truth <= k).then_some(truth);
                assert_eq!(got, want, "a={a:?} b={b:?} k={k} truth={truth}");
            }
        }
    }

    // The other half of the same guarantee, and the one the index rests on:
    // the *count filter* must never reject a pair that is genuinely within
    // `k`. If it can, matches vanish with no error anywhere.
    #[test]
    fn the_count_filter_never_rejects_a_true_match() {
        use crate::fuzzy::Fuzzy;

        let alphabet = ['a', 'b', 'c', 'd'];
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..4000 {
            let la = (next() % 10) as usize;
            let lb = (next() % 10) as usize;
            let a: String =
                (0..la).map(|_| alphabet[(next() % 4) as usize]).collect();
            let b: String =
                (0..lb).map(|_| alphabet[(next() % 4) as usize]).collect();

            let (na, nb) =
                (normalize(&a, Fold::Latin), normalize(&b, Fold::Latin));
            let truth = brute(&na, &nb);
            let (ga, gb) =
                (gram_prefixes(&na, DEFAULT_N), gram_prefixes(&nb, DEFAULT_N));
            let shared = ga.intersection(&gb).count();

            for k in 0..=5 {
                if truth > k {
                    continue;
                }
                let threshold =
                    Fuzzy::distance(k).threshold(ga.len(), DEFAULT_N);
                assert!(
                    shared >= threshold,
                    "'{a}' vs '{b}' are {truth} apart (k={k}) but share only \
                     {shared} of {} grams — the filter would have dropped a \
                     true match (threshold {threshold})",
                    ga.len()
                );
                // And the free length rejection must agree with it.
                assert!(
                    Fuzzy::distance(k)
                        .allows_length_gap(na.len().abs_diff(nb.len())),
                    "'{a}' vs '{b}': the length filter dropped a true match"
                );
            }
        }
    }

    #[test]
    fn jaccard_is_symmetric_and_bounded() {
        let g = |s: &str| gram_prefixes(&normalize(s, Fold::Latin), DEFAULT_N);
        let (a, b) = (g("smith"), g("smyth"));
        assert!((jaccard(&a, &a) - 1.0).abs() < f64::EPSILON);
        assert!((jaccard(&a, &b) - jaccard(&b, &a)).abs() < f64::EPSILON);
        assert!(jaccard(&a, &b) > 0.0 && jaccard(&a, &b) < 1.0);
        // Two empty sets are a match, not a division by zero.
        assert!((jaccard(&g(""), &g("")) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn rank_is_ascending_for_both_modes() {
        // One ordering rule, so the sort never branches on the mode.
        let near = Scored::at_distance("a", 1);
        let far = Scored::at_distance("b", 4);
        assert!(near.rank < far.rank);

        let good = Scored::at_similarity("a", 0.9);
        let poor = Scored::at_similarity("b", 0.2);
        assert!(good.rank < poor.rank);
        assert!((good.score - 0.9).abs() < f64::EPSILON);
    }
}
