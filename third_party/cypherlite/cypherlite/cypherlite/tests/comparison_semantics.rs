//! Comparison-operator semantics from the 2026 corpus that the engine
//! answered differently: `=~` is a whole-string match, maps order (size,
//! then keys, then values), durations and UUIDs have equality but no
//! ordering (so `<=`/`>=` still hold for equal values), and temporal
//! equality and ordering are offset/zone-sensitive. Each block pins the
//! exact expected/actual pair from the diagnosis.

mod common;

use cypherlite::Value;

use common::{assert_comparison_cases, connection, row};

// `=~` matches the whole string (Neo4j's `Pattern.matches`), which is what
// the index and non-index predicate paths both read.
#[test]
fn regex_matches_the_whole_string() {
    let mut c = connection();
    let r = row(
        &mut c,
        "RETURN '21_val' =~ '1_.*' AS m, '1_val' =~ '1_.*' AS hit, \
                'ab' =~ 'a|b' AS alt, 'a' =~ 'a|b' AS altHit",
    );
    assert_eq!(
        r,
        vec![
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(true)
        ]
    );

    // The same predicate over stored rows: only the two full matches.
    c.query("UNWIND range(1, 100) AS x CREATE (:User {prop: x + '_val'})")
        .unwrap();
    let matched = row(
        &mut c,
        "MATCH (u:User) WHERE u.prop =~ '1_.*' RETURN count(u) AS n",
    );
    assert_eq!(matched, vec![Value::Int(1)]);
}

// Maps compare by size, then sorted keys, then values; a null value under a
// shared key makes the whole comparison null.
#[test]
fn map_comparison_is_a_total_order() {
    let mut c = connection();
    let r = row(
        &mut c,
        "RETURN {k: 42} < {k: 42} AS a, {k: 42} <= {k: 42} AS b, \
                {k: 42} < {k: 43} AS c, {k: 42} <= {k: 43} AS d, \
                {k: 42} > {k: 43} AS e, {k: 42} >= {k: 43} AS f",
    );
    assert_eq!(
        r,
        vec![
            Value::Bool(false),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(false),
            Value::Bool(false)
        ]
    );

    // A differing key decides the order without inspecting values; a null
    // under a shared key propagates.
    assert_eq!(
        row(
            &mut c,
            "RETURN {k1: 42} < {k2: 42} AS a, {k1: 42} < {k2: null} AS b",
        ),
        vec![Value::Bool(true), Value::Bool(true)]
    );
    assert_eq!(
        row(&mut c, "RETURN {k: 42} < {k: null} AS a"),
        vec![Value::Null]
    );
    // A map against a non-map stays incomparable.
    assert_eq!(row(&mut c, "RETURN {k1: 42} < 42 AS a"), vec![Value::Null]);
}

// Durations are equality-only: `<`/`>` are null while `<=`/`>=` hold for
// equal durations.
#[test]
fn duration_comparison_is_equality_only() {
    let mut c = connection();
    c.query(
        "CREATE (n:L {d: duration({years: 12, months: 5, days: 14}), \
                      d2: duration({years: 12, months: 7, days: 14})})",
    )
    .unwrap();
    assert_eq!(
        row(
            &mut c,
            "MATCH (n:L) RETURN n.d < n.d AS lt, n.d <= n.d AS lte, \
                            n.d > n.d AS gt, n.d >= n.d AS gte",
        ),
        vec![
            Value::Null,
            Value::Bool(true),
            Value::Null,
            Value::Bool(true)
        ]
    );
    assert_eq!(
        row(
            &mut c,
            "MATCH (n:L) RETURN n.d < n.d2 AS lt, n.d <= n.d2 AS lte, \
                            n.d > n.d2 AS gt, n.d >= n.d2 AS gte",
        ),
        vec![Value::Null, Value::Null, Value::Null, Value::Null]
    );
}

// An indexed range predicate answers exactly like the residual one, including
// for durations, whose `<=`/`>=` hold through the equality fallback even
// though `<`/`>` are null.
#[test]
fn indexed_range_predicate_agrees_with_the_residual() {
    let mut c = connection();
    c.query(
        "CREATE (:L {d: duration({days: 1})}), (:L {d: duration({days: 2})}), \
                (:L {d: duration({days: 3})})",
    )
    .unwrap();
    let residual = row(
        &mut c,
        "MATCH (n:L) WHERE n.d <= duration({days: 2}) RETURN count(n) AS c",
    );
    c.query("CREATE INDEX FOR (n:L) ON (n.d)").unwrap();
    assert_eq!(
        row(
            &mut c,
            "MATCH (n:L) WHERE n.d <= duration({days: 2}) RETURN count(n) AS c",
        ),
        residual
    );
    assert_eq!(residual, vec![Value::Int(1)]);
    assert_eq!(
        row(
            &mut c,
            "MATCH (n:L) WHERE n.d >= duration({days: 3}) RETURN count(n) AS c",
        ),
        vec![Value::Int(1)]
    );
    assert_eq!(
        row(
            &mut c,
            "MATCH (n:L) WHERE n.d < duration({days: 3}) RETURN count(n) AS c",
        ),
        vec![Value::Int(0)]
    );
}

// UUIDs are equality-only for comparison operators; ORDER BY still sorts
// them by their numeric value.
#[test]
fn uuid_comparison_is_equality_only() {
    let mut c = connection();
    assert_eq!(
        row(
            &mut c,
            "RETURN UUID(1, 2) > UUID(3, 4) AS gt, UUID(1, 2) < UUID(3, 4) AS lt, \
                    UUID(1, 2) >= UUID(1, 2) AS gte, UUID(1, 2) <= UUID(1, 2) AS lte, \
                    UUID(1, 2) > 'a' AS cross",
        ),
        vec![
            Value::Null,
            Value::Null,
            Value::Bool(true),
            Value::Bool(true),
            Value::Null
        ]
    );
    // The string-spelling constructor behaves identically: an unequal pair
    // is incomparable, an equal pair still satisfies `>=`/`<=`.
    assert_eq!(
        row(
            &mut c,
            "RETURN UUID('f066ecfc-5151-4121-8a36-b35e00d72918') > \
                    UUID('cb967eb4-4baa-4128-ae82-fe15dd8a994c') AS gt, \
                    UUID('cb967eb4-4baa-4128-ae82-fe15dd8a994c') < \
                    UUID('f066ecfc-5151-4121-8a36-b35e00d72918') AS lt, \
                    UUID('f066ecfc-5151-4121-8a36-b35e00d72918') >= \
                    UUID('f066ecfc-5151-4121-8a36-b35e00d72918') AS ge, \
                    UUID('f066ecfc-5151-4121-8a36-b35e00d72918') <= \
                    UUID('f066ecfc-5151-4121-8a36-b35e00d72918') AS le",
        ),
        vec![
            Value::Null,
            Value::Null,
            Value::Bool(true),
            Value::Bool(true)
        ]
    );

    // ORDER BY keeps the numeric order for both input spellings: one probe
    // per spelling, each with the values that spelling denotes.
    for (low, high, low_value, high_value) in [
        (
            "UUID(1, 2)",
            "UUID(3, 4)",
            u128::from(1u64) << 64 | 2,
            u128::from(3u64) << 64 | 4,
        ),
        (
            "UUID('00000000-0000-0000-0000-00000000000a')",
            "UUID('00000000-0000-0000-0000-00000000000b')",
            10,
            11,
        ),
    ] {
        let ordered = c
            .query(&format!("UNWIND [{high}, {low}] AS u RETURN u ORDER BY u"))
            .unwrap()
            .rows;
        assert_eq!(
            ordered,
            vec![vec![Value::Uuid(low_value)], vec![Value::Uuid(high_value)]],
            "{low} < {high}",
        );
    }
}

// Times: equality is local time + offset, ordering is UTC nanos then offset,
// so same-instant values are ordered and counted separately.
#[test]
fn time_equality_and_ordering_are_offset_sensitive() {
    let mut c = connection();
    let cases = [
        // (a, b, eq, lt, gt, distinct)
        (
            "time('12:00+01:00')",
            "time('12:00+01:00')",
            true,
            false,
            false,
            1,
        ),
        (
            "time('12:00+01:00')",
            "time('11:00Z')",
            false,
            false,
            true,
            2,
        ),
        // Same UTC clock time away from midnight: the order falls to the
        // offset.
        (
            "time('02:30+02:00')",
            "time('00:30Z')",
            false,
            false,
            true,
            2,
        ),
        // ... while across midnight the un-wrapped UTC nanos decide.
        (
            "time('00:30+02:00')",
            "time('22:30Z')",
            false,
            true,
            false,
            2,
        ),
        (
            "time('00:30-01:00')",
            "time('01:30Z')",
            false,
            true,
            false,
            2,
        ),
        (
            "time('12:00Z')",
            "time('12:00+00:00')",
            true,
            false,
            false,
            1,
        ),
    ];
    assert_comparison_cases(&mut c, &cases);
}

// Datetimes: equality is local date-time + zone identity (a DST fall-back
// overlap is equal yet ordered), ordering breaks instant ties on the offset
// and then the zone.
#[test]
fn datetime_equality_and_ordering_are_zone_sensitive() {
    let mut c = connection();
    let cases = [
        // (a, b, eq, lt, gt, distinct)
        (
            "datetime('2020-01-01T12:00+01:00')",
            "datetime('2020-01-01T11:00Z')",
            false,
            false,
            true,
            2,
        ),
        (
            "datetime('2020-01-01T12:00+01:00')",
            "datetime('2020-01-01T12:00[Europe/Stockholm]')",
            false,
            true,
            false,
            2,
        ),
        (
            "datetime('2020-01-01T12:00[Europe/Berlin]')",
            "datetime('2020-01-01T12:00[Europe/Stockholm]')",
            false,
            true,
            false,
            2,
        ),
        (
            "datetime('2020-01-01T12:00[Europe/Kiev]')",
            "datetime('2020-01-01T12:00[Europe/Kyiv]')",
            false,
            true,
            false,
            2,
        ),
        (
            "datetime('2020-10-25T02:30+02:00[Europe/Stockholm]')",
            "datetime('2020-10-25T02:30+01:00[Europe/Stockholm]')",
            true,
            true,
            false,
            2,
        ),
        (
            "datetime('2020-01-01T12:00Z')",
            "datetime('2020-01-01T12:00+00:00')",
            true,
            false,
            false,
            1,
        ),
    ];
    assert_comparison_cases(&mut c, &cases);
}
