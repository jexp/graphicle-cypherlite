//! Uniqueness constraints compare constrained values with Cypher equality
//! (`Value::eq_non_null`): the exact Int/Float rule pinned by
//! `property_index_numeric_equivalence.rs`. These pins were written against
//! the full-scan validator before ADR-0172 made validation incremental; the
//! incremental probe must accept and reject exactly the same pairs.

mod common;

use cypherlite::Value;

use common::{connection, err_text, scalar, uniqueness};

const COUNT_P: &str = "MATCH (n:P) RETURN count(n) AS c";

fn unique_p_id() -> cypherlite::Connection {
    let mut c = connection();
    c.query("CREATE CONSTRAINT p_id FOR (n:P) REQUIRE n.id IS UNIQUE")
        .unwrap();
    c
}

/// Each pair of literals denotes the same constrained value, so the second
/// `CREATE` conflicts with the first and the graph keeps one node.
fn equivalent_pairs_conflict(pairs: &[(&str, &str)]) {
    for (first, second) in pairs {
        let mut c = unique_p_id();
        c.query(&format!("CREATE (:P {{id: {first}}})")).unwrap();
        let err = err_text(c.query(&format!("CREATE (:P {{id: {second}}})")));
        assert_eq!(err, uniqueness("p_id"), "{first} vs {second}");
        assert_eq!(
            scalar(&mut c, COUNT_P),
            Value::Int(1),
            "{first} vs {second}"
        );
    }
}

#[test]
fn int_and_float_denoting_the_same_number_conflict() {
    equivalent_pairs_conflict(&[
        ("1", "1.0"),
        ("1.0", "1"),
        ("[1]", "[1.0]"),
        ("[1.0]", "[1]"),
    ]);
}

#[test]
fn int_and_float_conflict_within_one_statement() {
    let mut c = unique_p_id();
    let err = err_text(c.query("CREATE (:P {id: 1}), (:P {id: 1.0})"));
    assert_eq!(err, uniqueness("p_id"));
    assert_eq!(scalar(&mut c, COUNT_P), Value::Int(0));
}

#[test]
fn int_and_nearby_float_past_2_pow_53_do_not_conflict() {
    let mut c = unique_p_id();
    c.query("CREATE (:P {id: 9007199254740993})").unwrap();
    c.query("CREATE (:P {id: 9007199254740992.0})").unwrap();
    assert_eq!(scalar(&mut c, COUNT_P), Value::Int(2));
    // The float does conflict with the integer it exactly denotes.
    let err = err_text(c.query("CREATE (:P {id: 9007199254740992})"));
    assert_eq!(err, uniqueness("p_id"));
    assert_eq!(scalar(&mut c, COUNT_P), Value::Int(2));
}

#[test]
fn nan_never_conflicts() {
    let mut c = unique_p_id();
    c.query("CREATE (:P {id: nan})").unwrap();
    c.query("CREATE (:P {id: nan})").unwrap();
    c.query("CREATE (:P {id: nan}), (:P {id: nan})").unwrap();
    assert_eq!(scalar(&mut c, COUNT_P), Value::Int(4));
}

#[test]
fn composite_tuples_with_a_missing_property_never_conflict() {
    let mut c = connection();
    c.query("CREATE CONSTRAINT p_ab FOR (n:P) REQUIRE (n.a, n.b) IS UNIQUE")
        .unwrap();
    c.query("CREATE (:P {a: 1}), (:P {a: 1})").unwrap();
    c.query("CREATE (:P {a: 1})").unwrap();
    c.query("CREATE (:P {a: 1, b: 2})").unwrap();
    let err = err_text(c.query("CREATE (:P {a: 1.0, b: 2.0})"));
    assert_eq!(err, uniqueness("p_ab"));
    assert_eq!(scalar(&mut c, COUNT_P), Value::Int(4));
}

/// `Temporal::compare` (which `Value::eq_non_null` uses for temporals)
/// compares zoned datetimes by their UTC instant, ignoring the offset and
/// zone name; `GroupKey::for_property_index` buckets on the same instant.
#[test]
fn datetimes_at_the_same_instant_with_different_offsets_conflict() {
    let mut c = unique_p_id();
    c.query("CREATE (:P {id: datetime('2024-01-01T12:00:00+01:00')})")
        .unwrap();
    // 12:00 at +01:00 is 11:00 UTC.
    let err = err_text(c.query("CREATE (:P {id: datetime('2024-01-01T11:00:00Z')})"));
    assert_eq!(err, uniqueness("p_id"));
    // A genuinely different instant is accepted.
    c.query("CREATE (:P {id: datetime('2024-01-01T12:00:00Z')})")
        .unwrap();
    assert_eq!(scalar(&mut c, COUNT_P), Value::Int(2));
}

/// `Temporal::compare` normalizes a zoned time to nanos since midnight UTC
/// (`local time - offset`); the equivalence key does the same.
#[test]
fn times_at_the_same_instant_with_different_offsets_conflict() {
    let mut c = unique_p_id();
    c.query("CREATE (:P {id: time('12:00+01:00')})").unwrap();
    // 12:00 at +01:00 and 13:00 at +02:00 are both 11:00 UTC.
    let err = err_text(c.query("CREATE (:P {id: time('13:00+02:00')})"));
    assert_eq!(err, uniqueness("p_id"));
    // A genuinely different time is accepted.
    c.query("CREATE (:P {id: time('12:00Z')})").unwrap();
    assert_eq!(scalar(&mut c, COUNT_P), Value::Int(2));
}

/// IEEE `==` equates `-0.0` and `0.0`, and the exact Int/Float rule equates
/// both with `0`; the equivalence key normalizes all three to `Int(0)`.
#[test]
fn negative_zero_and_zero_conflict() {
    equivalent_pairs_conflict(&[
        ("0.0", "-0.0"),
        ("-0.0", "0.0"),
        ("0", "-0.0"),
        ("-0.0", "0"),
    ]);
}

/// Points compare coordinate-wise with `==` (`Value::eq_non_null`'s Point
/// arm), so a `-0.0` coordinate equals a `0.0` one; the equivalence key
/// normalizes both to `Int(0)` per coordinate.
#[test]
fn points_with_negative_zero_coordinates_conflict() {
    let mut c = unique_p_id();
    c.query("CREATE (:P {id: point({x: 0.0, y: 0.0})})")
        .unwrap();
    let err = err_text(c.query("CREATE (:P {id: point({x: -0.0, y: 0.0})})"));
    assert_eq!(err, uniqueness("p_id"));
    // A genuinely different point is accepted.
    c.query("CREATE (:P {id: point({x: 0.0, y: 1.0})})")
        .unwrap();
    assert_eq!(scalar(&mut c, COUNT_P), Value::Int(2));
}
