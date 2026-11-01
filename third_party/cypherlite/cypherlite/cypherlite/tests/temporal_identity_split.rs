//! The temporal identity model (ADR-0446): a zoned datetime has **three**
//! relations, and each is the one its consumer needs.
//!
//! * `=` (`Temporal::equals`) compares the local wall clock plus the zone
//!   identity, so the two halves of a DST fall-back overlap are equal.
//! * `<`/ordering (`Temporal::compare`) compares instants, so the same two
//!   halves are ordered — equality does not imply "neither is less".
//! * `DISTINCT`/grouping (`GroupKey`) compares the UTC instant plus the zone,
//!   so the pair stays two groups — the spec's documented equality/
//!   equivalence divergence.
//!
//! Everything that *buckets* values by equality — an indexed seek, `MERGE`,
//! a uniqueness constraint — must therefore coarsen `=`, never the grouping
//! relation. Before this pin, the seek bucket was instant-keyed: a
//! uniqueness constraint accepted the equal partner and `MERGE` created a
//! second node whenever such a constraint (or a range index) served the
//! lookup. These tests pin all four verdicts on the same pair.

mod common;

use cypherlite::Value;

use common::{
    assert_comparison_cases, connection, err_text, scalar, unique_constrained, uniqueness,
};

const FIRST: &str = "datetime('2025-10-26T02:30:00+02:00[Europe/Stockholm]')";
const SECOND: &str = "datetime('2025-10-26T02:30:00+01:00[Europe/Stockholm]')";

const COUNT_X: &str = "MATCH (n:X) RETURN count(n) AS c";

/// The pair is equal and ordered, yet two equivalence classes.
#[test]
fn dst_overlap_halves_are_equal_ordered_and_distinct() {
    let mut c = connection();
    assert_comparison_cases(&mut c, &[(FIRST, SECOND, true, true, false, 2)]);
}

/// Uniqueness is the `=` relation: the second half conflicts, whichever
/// order the two are created in.
#[test]
fn unique_constraint_rejects_the_equal_half() {
    for (first, second) in [(FIRST, SECOND), (SECOND, FIRST)] {
        let mut c = unique_constrained("x_dt", "X", "dt");
        c.query(&format!("CREATE (:X {{dt: {first}}})")).unwrap();
        let err = err_text(c.query(&format!("CREATE (:X {{dt: {second}}})")));
        assert_eq!(err, uniqueness("x_dt"), "{first} then {second}");
        assert_eq!(scalar(&mut c, COUNT_X), Value::Int(1));
    }
}

/// The full re-check behind `CREATE CONSTRAINT` buckets by the same key as
/// the incremental probe, so a store already holding the pair rejects the
/// constraint.
#[test]
fn creating_the_constraint_rejects_an_existing_equal_pair() {
    let mut c = connection();
    c.query(&format!("CREATE (:X {{dt: {FIRST}}})")).unwrap();
    c.query(&format!("CREATE (:X {{dt: {SECOND}}})")).unwrap();
    let err = err_text(c.query("CREATE CONSTRAINT x_dt FOR (n:X) REQUIRE n.dt IS UNIQUE"));
    assert_eq!(err, uniqueness("x_dt"));
    assert_eq!(scalar(&mut c, COUNT_X), Value::Int(2));
}

/// `MERGE` matches the equality relation even when the uniqueness
/// constraint's backing index serves the lookup: it finds the equal node
/// instead of creating a second one.
#[test]
fn merge_matches_the_equal_half_under_a_unique_constraint() {
    let mut c = unique_constrained("x_dt", "X", "dt");
    c.query(&format!("MERGE (:X {{dt: {FIRST}}})")).unwrap();
    c.query(&format!("MERGE (:X {{dt: {SECOND}}})")).unwrap();
    assert_eq!(scalar(&mut c, COUNT_X), Value::Int(1));
}

/// `Temporal::equals` (which `Value::eq_non_null` uses for temporals) and
/// `GroupKey` both compare a zoned datetime by its local date-time and zone
/// identity, so the same instant in a different offset is a different value.
#[test]
fn datetimes_at_the_same_instant_with_different_offsets_do_not_conflict() {
    let mut c = unique_constrained("x_dt", "X", "dt");
    c.query("CREATE (:X {dt: datetime('2024-01-01T12:00:00+01:00')})")
        .unwrap();
    // 12:00 at +01:00 is 11:00 UTC, but the local date-times differ.
    c.query("CREATE (:X {dt: datetime('2024-01-01T11:00:00Z')})")
        .unwrap();
    assert_eq!(scalar(&mut c, COUNT_X), Value::Int(2));
    // A distinct local date-time is accepted; its exact repeat conflicts.
    c.query("CREATE (:X {dt: datetime('2024-01-01T12:00:00+02:00')})")
        .unwrap();
    let err = err_text(c.query("CREATE (:X {dt: datetime('2024-01-01T12:00:00+02:00')})"));
    assert_eq!(err, uniqueness("x_dt"));
    assert_eq!(scalar(&mut c, COUNT_X), Value::Int(3));
}

/// `Temporal::equals` and the equivalence key both compare a time by its
/// local time **and** offset: 12:00+01:00 and 13:00+02:00 share an instant
/// but are different times.
#[test]
fn times_at_the_same_instant_with_different_offsets_do_not_conflict() {
    let mut c = unique_constrained("x_dt", "X", "dt");
    c.query("CREATE (:X {dt: time('12:00+01:00')})").unwrap();
    // 12:00 at +01:00 and 13:00 at +02:00 are both 11:00 UTC.
    c.query("CREATE (:X {dt: time('13:00+02:00')})").unwrap();
    assert_eq!(scalar(&mut c, COUNT_X), Value::Int(2));
    // `Z` and `+00:00` spell one time, so the second spelling conflicts.
    c.query("CREATE (:X {dt: time('12:00+00:00')})").unwrap();
    let err = err_text(c.query("CREATE (:X {dt: time('12:00Z')})"));
    assert_eq!(err, uniqueness("x_dt"));
    assert_eq!(scalar(&mut c, COUNT_X), Value::Int(3));
}

/// An equality seek served by a range index returns every `=`-equal node,
/// exactly like the unindexed scan does.
#[test]
fn indexed_seek_finds_the_equal_half() {
    let mut c = connection();
    c.query("CREATE RANGE INDEX x_dt_range FOR (n:X) ON (n.dt)")
        .unwrap();
    c.query(&format!("CREATE (:X {{dt: {SECOND}}})")).unwrap();
    // Pattern-map equality, `WHERE` equality and `IN` all resolve the pair.
    for q in [
        format!("MATCH (n:X {{dt: {FIRST}}}) RETURN count(n) AS c"),
        format!("MATCH (n:X) WHERE n.dt = {FIRST} RETURN count(n) AS c"),
        format!("MATCH (n:X) WHERE n.dt IN [{FIRST}] RETURN count(n) AS c"),
    ] {
        assert_eq!(scalar(&mut c, &q), Value::Int(1), "{q}");
    }
    // A genuinely different local date-time stays a miss.
    assert_eq!(
        scalar(
            &mut c,
            "MATCH (n:X {dt: datetime('2025-10-26T02:30:00+02:00[Europe/Berlin]')}) RETURN count(n) AS c"
        ),
        Value::Int(0)
    );
}
