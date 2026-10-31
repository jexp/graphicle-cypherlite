//! The exact `INTEGER`/`FLOAT` comparison rule — one rule, one home.
//!
//! Cypher compares numbers by **mathematical value**, ignoring the
//! Int/Float distinction: `1 = 1.0`, `0.0 = -0.0`. A naive
//! `(i as f64) == f` cast implements that incorrectly: for `|i| > 2^53`
//! the cast rounds away low bits, so `9007199254740993 as f64 ==
//! 9007199254740992.0` spuriously holds even though the two denote
//! different numbers. The exact rule instead compares in the integer
//! domain, guarded by the round-trip range: `Int(i)` and `Float(f)` are
//! equal iff `f` is integral, lies in the exact-integer range
//! `[-2^63, 2^63)`, and denotes exactly `i` (see [`f64_exact_i64`]).
//! Ground-truthed against live Neo4j (Cypher 25), whose `numbersEqual`
//! likewise requires the double to reproduce the long exactly.
//!
//! Three consumers must never drift apart, so all three route through
//! this module:
//!
//! * `CypherValue`'s `PartialEq`/`PartialOrd` (`value.rs`, this crate);
//! * the engine's `=`/`<` evaluation — `cypherlite_core::Value`'s
//!   `eq_non_null`/`compare` call [`eq_i64_f64`]/[`cmp_i64_f64`];
//! * the executor's equivalence keys (`group_key::GroupKey`, backing
//!   `DISTINCT`/`GROUP BY`/`UNION` dedup/`count(DISTINCT …)` and the
//!   property index), which normalize floats through [`f64_exact_i64`].
//!
//! Because all three share the rule, an indexed `{prop: value}` seek, a
//! `WHERE` residual, and a `DISTINCT` bucket can never disagree — the
//! divergence that used to let an index's presence change a MATCH/MERGE
//! answer for a large-magnitude integer paired with a nearby float.
//!
//! This module owns only the Int/Float rule. Cypher's wider *equivalence*
//! relation (`null ≡ null`, per-occurrence NaN keys, structural rules for
//! lists/maps/temporals/vectors/…) lives with its consumer:
//! `cypherlite-executor`'s `group_key` module keys the engine's
//! dedup/grouping/index sites.

use std::cmp::Ordering;

// `-2^63` and `2^63` are both exactly representable as `f64`. `i64::MAX`
// (`2^63 - 1`) is *not* -- it rounds up to `2^63` -- so the exact-integer
// range for a float is the half-open interval `[-2^63, 2^63)`. Any integral
// float in that interval round-trips through `as i64` losslessly; anything
// at or beyond `2^63` (or below `-2^63`) denotes no exact `i64`.
const I64_MIN_F: f64 = -9_223_372_036_854_775_808.0; // -2^63
const I64_MAX_EXCL_F: f64 = 9_223_372_036_854_775_808.0; // 2^63

/// The exact integer an `f64` denotes, if it denotes one: `Some(i)` iff `f`
/// is integral and lands in the exact-integer range `[-2^63, 2^63)`, in
/// which case `f as i64` is lossless. Returns `None` for non-integral
/// floats, NaN, ±inf, and out-of-range magnitudes (including `2^63` itself,
/// which would otherwise saturate-collide with `i64::MAX` under a plain
/// cast).
///
/// `-0.0` and `+0.0` are both integral and in range, so both yield
/// `Some(0)` — giving `0.0 = -0.0` for free. This is the primitive every
/// Int/Float equality, ordering, and numeric-key normalization in the
/// engine routes through (see the module doc).
pub fn f64_exact_i64(f: f64) -> Option<i64> {
    if f.fract() == 0.0 && (I64_MIN_F..I64_MAX_EXCL_F).contains(&f) {
        Some(f as i64)
    } else {
        None
    }
}

/// Exact equality between an `i64` and an `f64`: true iff `f` denotes
/// exactly `i` per [`f64_exact_i64`]. A plain `(i as f64) == f` is **wrong**
/// here: casting `i` to `f64` rounds away low bits for `|i| > 2^53`, so
/// `9007199254740993 as f64 == 9007199254740992.0` spuriously holds.
pub fn eq_i64_f64(i: i64, f: f64) -> bool {
    f64_exact_i64(f) == Some(i)
}

/// Exact ordering between an `i64` and an `f64`, consistent with
/// [`eq_i64_f64`]: it returns `Some(Ordering::Equal)` for exactly the pairs
/// [`eq_i64_f64`] calls equal, and never otherwise. Returns `None` only for
/// a NaN `f` (unordered), matching `f64::partial_cmp`. Keeping `=`/`<` (and
/// hence `ORDER BY`) mutually consistent even past `2^53` matters: a
/// hand-written equality and ordering that disagree on equality is a
/// latent-bug generator.
pub fn cmp_i64_f64(i: i64, f: f64) -> Option<Ordering> {
    if f.is_nan() {
        return None;
    }
    if let Some(fi) = f64_exact_i64(f) {
        return Some(i.cmp(&fi));
    }
    // `f` is non-integral, out of the exact-integer range, or ±inf.
    if f >= I64_MAX_EXCL_F {
        return Some(Ordering::Less); // i <= i64::MAX < 2^63 <= f
    }
    if f < I64_MIN_F {
        return Some(Ordering::Greater); // f < -2^63 <= i64::MIN <= i
    }
    // Non-integral `f` strictly inside the range: every `|f64| >= 2^52` is
    // integral, so `|f| < 2^52` here and `floor(f)` is exactly
    // representable, keeping the whole comparison in the integer domain
    // (no lossy `i as f64` cast anywhere). `i <= floor(f)` implies
    // `i < f` (a non-integral `f` strictly exceeds its floor); otherwise
    // `i >= floor(f) + 1 > f`. Never `Equal` — an integral `i` cannot
    // equal a non-integral `f` — matching `eq_i64_f64`.
    let fl = f.floor() as i64;
    Some(if i <= fl {
        Ordering::Less
    } else {
        Ordering::Greater
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn exact_integer_extraction() {
        assert_eq!(f64_exact_i64(1.0), Some(1));
        assert_eq!(f64_exact_i64(0.0), Some(0));
        assert_eq!(f64_exact_i64(-0.0), Some(0));
        assert_eq!(f64_exact_i64(1.5), None);
        assert_eq!(f64_exact_i64(-1.5), None);
        assert_eq!(f64_exact_i64(f64::NAN), None);
        assert_eq!(f64_exact_i64(f64::INFINITY), None);
        assert_eq!(f64_exact_i64(f64::NEG_INFINITY), None);
        // The exact-range corners: -2^63 is in, 2^63 is out.
        assert_eq!(f64_exact_i64(-9_223_372_036_854_775_808.0), Some(i64::MIN));
        assert_eq!(f64_exact_i64(9_223_372_036_854_775_808.0), None);
        // 2^62 (exactly representable, in range) round-trips.
        let two_pow_62: i64 = 4_611_686_018_427_387_904;
        assert_eq!(f64_exact_i64(two_pow_62 as f64), Some(two_pow_62));
        // 2^53 as a float is exact; 2^53 + 1 is not an f64 at all.
        assert_eq!(
            f64_exact_i64(9_007_199_254_740_992.0),
            Some(9_007_199_254_740_992)
        );
    }

    #[test]
    fn eq_is_exact_past_2_pow_53() {
        // 2^53 + 1 rounds to 2^53 as an f64; a lossy cast says EQUAL.
        assert!(!eq_i64_f64(9_007_199_254_740_993, 9_007_199_254_740_992.0));
        // The float DOES equal the integer it exactly represents.
        assert!(eq_i64_f64(9_007_199_254_740_992, 9_007_199_254_740_992.0));
        // Ordinary small values keep `1 = 1.0`.
        assert!(eq_i64_f64(1, 1.0));
        assert!(eq_i64_f64(0, -0.0));
        assert!(!eq_i64_f64(1, 1.5));
        // i64 extremes vs the adjacent floats: 2^63 is not i64::MAX.
        assert!(!eq_i64_f64(i64::MAX, 9_223_372_036_854_775_808.0));
        assert!(eq_i64_f64(i64::MIN, -9_223_372_036_854_775_808.0));
        // NaN equals nothing.
        assert!(!eq_i64_f64(0, f64::NAN));
    }

    #[test]
    fn cmp_is_exact_past_2_pow_53() {
        // 2^53 + 1 sits strictly above 2^53.
        assert_eq!(
            cmp_i64_f64(9_007_199_254_740_993, 9_007_199_254_740_992.0),
            Some(Greater)
        );
        assert_eq!(
            cmp_i64_f64(9_007_199_254_740_992, 9_007_199_254_740_992.0),
            Some(Equal)
        );
        // Non-integral floats order exactly, without any lossy cast.
        assert_eq!(cmp_i64_f64(1, 1.5), Some(Less));
        assert_eq!(cmp_i64_f64(2, 1.5), Some(Greater));
        assert_eq!(cmp_i64_f64(-1, -0.5), Some(Less));
        assert_eq!(cmp_i64_f64(0, -0.5), Some(Greater));
        assert_eq!(cmp_i64_f64(-2, -1.5), Some(Less));
        assert_eq!(cmp_i64_f64(0, -0.0), Some(Equal));
        // Out-of-range magnitudes (and ±inf) order by sign; never Equal.
        assert_eq!(
            cmp_i64_f64(i64::MAX, 9_223_372_036_854_775_808.0),
            Some(Less)
        );
        assert_eq!(cmp_i64_f64(i64::MIN, -1.0e19), Some(Greater));
        assert_eq!(cmp_i64_f64(i64::MAX, f64::INFINITY), Some(Less));
        assert_eq!(cmp_i64_f64(i64::MIN, f64::NEG_INFINITY), Some(Greater));
        // NaN is unordered.
        assert_eq!(cmp_i64_f64(0, f64::NAN), None);
    }

    #[test]
    fn eq_and_cmp_agree_across_the_boundary() {
        let probes = [
            (9_007_199_254_740_993_i64, 9_007_199_254_740_992.0_f64),
            (9_007_199_254_740_992, 9_007_199_254_740_992.0),
            (i64::MAX, 9_223_372_036_854_775_808.0), // 2^63, out of exact range
            (i64::MIN, -9_223_372_036_854_775_808.0), // -2^63, in exact range
            (0, -0.0),
            (1, 1.0),
            (2, 1.5),
            (1, 1.5),
            (-2, -1.5),
        ];
        for (i, f) in probes {
            assert_eq!(
                eq_i64_f64(i, f),
                cmp_i64_f64(i, f) == Some(Equal),
                "eq/cmp disagree for Int({i}) vs Float({f})"
            );
        }
    }
}
