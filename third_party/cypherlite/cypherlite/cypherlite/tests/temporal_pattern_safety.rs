//! Malformed temporal pattern inputs and out-of-range temporal arithmetic
//! must surface as query errors and must never unwind into the host
//! (memdb-merge/03-temporal-parse-safety and its repair).
//!
//! Covers the four historically verified panic inputs
//! (`datetime('abc', 'yyyy-MM-ddXXX')`, `date('abc', 'yyyy-MM-dd a')`,
//! `datetime('abc', 'pppyy-MM-dd')`,
//! `datetime('abc', "yyyy, DDD 'day of year' c 'day-of-week'")`)
//! plus one malformed variant per remaining `parse_pattern` arm, and the
//! reviewer-reproduced arithmetic overflows from the repair round
//! (`date('2024-01-01') + duration({months: 1000000000000})`,
//! `datetime({year: 2024}) + duration({days: 1e18})`, and
//! `duration.between` over ±9e18-year civil dates) with their sign/variant
//! siblings.

use std::panic::{catch_unwind, AssertUnwindSafe};

use cypherlite::Database;

const MALFORMED_QUERIES: &[&str] = &[
    // Verified panic inputs.
    r#"RETURN datetime('abc', 'yyyy-MM-ddXXX')"#,
    r#"RETURN date('abc', 'yyyy-MM-dd a')"#,
    r#"RETURN datetime('abc', 'pppyy-MM-dd')"#,
    r#"RETURN datetime("abc", "yyyy, DDD 'day of year' c 'day-of-week'")"#,
    // One malformed variant per remaining parse_pattern arm.
    r#"RETURN duration('abc', 'yyyy-MM-dd')"#,
    r#"RETURN date('2015-01-01', 'JJ')"#,
    r#"RETURN datetime('2015-01', 'yyyy-MM')"#,
    r#"RETURN localtime('abc def', 'H G')"#,
    r#"RETURN localdatetime('abc', 'yyyy-MM-dd VV')"#,
    r#"RETURN date('abc', 'yyyy-MM-dd EEEE')"#,
    // Civil (out-of-range) date hits the former `unreachable!()`.
    r#"RETURN date('2015-17-21', 'yyyy-MM-dd EEEE')"#,
    r#"RETURN datetime("abc", "EEEE, MMMM d, G uuuu")"#,
    r#"RETURN datetime('abc', 'yyyy-MM-dd k:mm z')"#,
    r#"RETURN datetime('abc', 'yyyy-MM-dd K:mm a O')"#,
    r#"RETURN datetime('abc', 'yyyy LLL d, A m s')"#,
    r#"RETURN datetime("abc", "'Nano:' n 'of day:' N yyyy-MM-dd")"#,
    r#"RETURN datetime('abc', 'yyyy-MM-dd hh:mm a VV B')"#,
    r#"RETURN datetime("abc", "YYYY-'W'ww-e")"#,
    r#"RETURN datetime("abc", "QQQQ yyyy 'day': D")"#,
    // Compact arms: wrong length, and a multi-byte value whose byte length
    // matches the guard but whose slices are not char boundaries.
    r#"RETURN date('201501a', 'yyyyMMdd')"#,
    "RETURN date('201\u{e5}071', 'yyyyMMdd')",
    r#"RETURN localtime('12345678', 'HHmmssSSS')"#,
    r#"RETURN localtime('1234x', 'HHmmss')"#,
    r#"RETURN localdatetime('201501011210', 'yyyyMMddHHmmss')"#,
    r#"RETURN datetime('2015010112101x', 'yyyyMMddHHmmssZ')"#,
    r#"RETURN time('99xx99+01', 'HHmmssZ')"#,
    // parse_iso fallback paths reachable from parse_pattern.
    "RETURN time('\u{e5}', 'HH')",
    "RETURN date('2015W5\u{e5}5', 'yyyy')",
    r#"RETURN date('not-a-date', 'yyyy')"#,
];

/// Out-of-range temporal arithmetic (repair round): the three reviewer-
/// reproduced panics, sign/variant siblings, and the further user-reachable
/// overflows found in the whole-module re-sweep of `temporal/`. Every query
/// here panicked in a debug build before the repair.
const OVERFLOW_QUERIES: &[&str] = &[
    // Reviewer repro 1: add_months unwrap on out-of-chrono-range year.
    r#"RETURN date('2024-01-01') + duration({months: 1000000000000})"#,
    r#"RETURN date('2024-01-01') + duration({months: -1000000000000})"#,
    r#"RETURN date('2024-01-01') - duration({months: 1000000000000})"#,
    // Reviewer repro 2: TimeDelta overflow in add_duration, on each
    // date-carrying variant and on subtraction.
    r#"RETURN datetime({year: 2024}) + duration({days: 1e18})"#,
    r#"RETURN datetime({year: 2024}) - duration({days: 1e18})"#,
    r#"RETURN localdatetime('2024-01-01T00:00') + duration({days: 1e18})"#,
    r#"RETURN date('2024-01-01') + duration({days: 1e18})"#,
    r#"RETURN datetime({year: 2024}) + duration({seconds: 1e30})"#,
    // Reviewer repro 3: i64 overflow in `(yb - ya) * 12` for civil years.
    r#"RETURN duration.between(date('-9000000000000000000-01-01'), date('+9000000000000000000-01-01'))"#,
    r#"RETURN duration.between(date('+9000000000000000000-01-01'), date('-9000000000000000000-01-01'))"#,
    r#"RETURN duration.inMonths(date('-9000000000000000000-01-01'), date('+9000000000000000000-01-01'))"#,
    r#"RETURN duration.inDays(date('-9000000000000000000-01-01'), date('+9000000000000000000-01-01'))"#,
    r#"RETURN duration.inSeconds(date('+9000000000000000000-01-01'), date('-9000000000000000000-01-01'))"#,
    // Duration component accumulation overflows (duration+duration, ISO
    // duration parsing, negation of i64::MIN components).
    r#"RETURN duration({months: 9223372036854775807}) + duration({months: 1})"#,
    r#"RETURN duration({days: 9223372036854775807}) + duration({days: 1})"#,
    r#"RETURN duration('P1000000000000000000000000Y')"#,
    r#"RETURN duration('P9000000000000000000D9000000000000000000D')"#,
    r#"RETURN duration('P9000000000000000000-01-01')"#,
    r#"RETURN duration('PT90000000000000000000000000H90000000000000000000000000M')"#,
    r#"RETURN duration('-P9000000000000000000Y')"#,
    // Map-driven u32/i64 overflows in construction and truncation extras.
    r#"RETURN time({hour: 1, millisecond: 5000000000})"#,
    r#"RETURN time({hour: 1, millisecond: -1})"#,
    r#"RETURN date({year: 2024, quarter: 2000000000})"#,
    r#"RETURN date({year: 2024, quarter: 2000000000, dayOfQuarter: 1})"#,
    r#"RETURN date.truncate('week', date('2024-01-01'), {dayOfWeek: 9000000000000000000})"#,
    // Offset subtraction at the chrono-range boundary, and offset parsing
    // beyond i32.
    r#"RETURN datetime({year: -262143, month: 1, day: 1, timezone: '+99:99'})"#,
    r#"RETURN time('12:00+2147483647:00')"#,
    // Pattern-parsed millis-of-day at i64::MIN (TimeDelta::milliseconds).
    "RETURN datetime('2015 Jan 1, -9223372036854775808 0 0', 'yyyy LLL d, A m s')",
    // Boundary-crossing arithmetic and out-of-range scaling/construction.
    r#"RETURN datetime({year: 262142, month: 12, day: 31}) + duration({days: 100})"#,
    r#"RETURN duration({months: 5}) * 1e300"#,
    r#"RETURN duration({years: 1e300})"#,
    // Repair round 2, reviewer repro 1: truncate('week', ...) at the chrono
    // lower bound — chrono's `NaiveDate - TimeDelta` panicked on underflow.
    // -262143-01-01 is a Thursday, so Jan 1-4 all underflow; every kind
    // routes through the same truncate_date.
    r#"RETURN date.truncate('week', date('-262143-01-01'))"#,
    r#"RETURN date.truncate('week', date('-262143-01-02'))"#,
    r#"RETURN date.truncate('week', date('-262143-01-03'))"#,
    r#"RETURN date.truncate('week', date('-262143-01-04'))"#,
    r#"RETURN localdatetime.truncate('week', localdatetime('-262143-01-01T00:00'))"#,
    r#"RETURN datetime.truncate('week', datetime('-262143-01-01T00:00Z'))"#,
    // weekyear sibling: ISO week-1 Monday of year -262143 precedes
    // NaiveDate::MIN.
    r#"RETURN date.truncate('weekyear', date('-262143-01-01'))"#,
    // Repair round 2, reviewer repro 2: unary minus on a duration whose
    // months == i64::MIN (civil duration.between) and on the extracted
    // i64::MIN integer component — both eval.rs unary-minus arms.
    r#"RETURN -duration.between(date('+768614336404564650-09-01'), date('+0000-01-01'))"#,
    r#"RETURN -duration.inMonths(date('+768614336404564650-09-01'), date('+0000-01-01'))"#,
    r#"RETURN -(duration.between(date('+768614336404564650-09-01'), date('+0000-01-01')).months)"#,
    r#"RETURN -(duration.inMonths(date('+768614336404564650-09-01'), date('+0000-01-01')).months)"#,
    // Repair round 2, reviewer low: truncating i128 -> i64 casts on extreme
    // duration nanos returned garbage integers; now honest range errors.
    r#"RETURN duration({seconds: 1e28}).minutes"#,
    r#"RETURN duration({seconds: 1e28}).hours"#,
    r#"RETURN duration({seconds: 1e28}).seconds"#,
    r#"RETURN duration({seconds: 1e28}).nanoseconds"#,
    r#"RETURN duration({seconds: 1e28}).milliseconds"#,
    r#"RETURN duration({seconds: 1e28}).microseconds"#,
    r#"RETURN duration({seconds: -1e28}).minutes"#,
    // Date-arm extra_days cast on a beyond-i64 day-equivalent nanos count.
    r#"RETURN date('2024-01-01') + duration({seconds: 1e28})"#,
    r#"RETURN localdatetime('2024-01-01T00:00') + duration({seconds: 1e28})"#,
    // Repair round 2 sweep find: chrono's from_isoywd_opt computes `year ± 1`
    // internally before its range guard; i32-extreme years (via truncating
    // map casts) panicked inside chrono. Now rejected before the call.
    r#"RETURN date({year: 2147483648, week: 1})"#,
    r#"RETURN date({year: -2147483649, week: 53})"#,
    r#"RETURN date({year: 2147483647, week: 52, dayOfWeek: 6})"#,
    r#"RETURN localdatetime({year: 2147483648, week: 1})"#,
    r#"RETURN datetime({year: 2147483648, week: 1})"#,
    r#"RETURN date('2147483647W526')"#,
    r#"RETURN date('-2147483648W011')"#,
    // Repair round 2 sweep finds in the negation-of-extreme class:
    // a user-parsed offset can be exactly i32::MIN (-(596523*3600+14*60+8)),
    // and `-*offset` in duration.between's zone-context path panicked on it.
    r#"RETURN duration.between(time('12:00-596523:14:08'), datetime('2015-01-01T00:00[Europe/Stockholm]'))"#,
    r#"RETURN duration.between(datetime('2015-01-01T00:00[Europe/Stockholm]'), time('12:00-596523:14:08'))"#,
    r#"RETURN duration.inSeconds(time('12:00-596523:14:08'), datetime('2015-01-01T00:00[Europe/Stockholm]'))"#,
    // Truncating map/civil casts that wrapped extreme values into range and
    // silently produced wrong dates/times; now honest range errors.
    r#"RETURN date.truncate('day', date('+4294967297-01-01'))"#,
    r#"RETURN date({year: 4294967297})"#,
    r#"RETURN date({year: 2024, month: 4294967297})"#,
    r#"RETURN date({year: 2024, day: 4294967297})"#,
    r#"RETURN time({hour: 4294967297})"#,
    r#"RETURN date({year: 2024, ordinalDay: 4294967297})"#,
];

fn negative_queries() -> impl Iterator<Item = &'static str> {
    MALFORMED_QUERIES
        .iter()
        .copied()
        .chain(OVERFLOW_QUERIES.iter().copied())
}

#[test]
fn malformed_temporal_pattern_queries_error_and_do_not_panic() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    for query in negative_queries() {
        let result = conn.query(query);
        assert!(result.is_err(), "query must fail with an error: {query}");
    }
}

/// No unwind may escape the public query path for those inputs. A fresh
/// database per query keeps a hypothetically poisoned engine from masking a
/// later panic.
#[test]
fn temporal_pattern_queries_do_not_unwind_through_public_api() {
    for query in negative_queries() {
        let mut conn = Database::open(":memory:").unwrap().connect().unwrap();
        let outcome = catch_unwind(AssertUnwindSafe(|| conn.query(query)));
        let result = outcome.unwrap_or_else(|_| panic!("query panicked: {query}"));
        assert!(result.is_err(), "query must fail with an error: {query}");
    }
}

/// Boundary values whose truncated/computed result is representable keep
/// working through the public query path (repair round 2 positive controls).
#[test]
fn temporal_boundary_values_still_work() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let controls: &[(&str, &str)] = &[
        // -262143-01-05 is the first Monday inside chrono range.
        (
            "RETURN date.truncate('week', date('-262143-01-06'))",
            "-262143-01-05",
        ),
        (
            "RETURN date.truncate('week', date('-262143-01-05'))",
            "-262143-01-05",
        ),
        (
            "RETURN date.truncate('day', date('-262143-01-01'))",
            "-262143-01-01",
        ),
        (
            "RETURN date.truncate('month', date('-262143-01-01'))",
            "-262143-01-01",
        ),
        // Week truncation at the upper bound: 262142-12-31 (NaiveDate::MAX)
        // is itself a Monday.
        (
            "RETURN date.truncate('week', date('262142-12-31'))",
            "+262142-12-31",
        ),
        (
            "RETURN date.truncate('week', date('262142-12-30'))",
            "+262142-12-24",
        ),
        // duration.inSeconds across the full chrono span: previously a
        // silent PT0S via `num_nanoseconds().unwrap_or(0)`.
        (
            "RETURN duration.inSeconds(datetime('-262143-01-01T00:00Z'), datetime({year: 262142, month: 12, day: 31, hour: 23, minute: 59, second: 59, nanosecond: 999999999}))",
            "PT4595796695H59M59.999999999S",
        ),
        // Small-duration components stay exact.
        ("RETURN duration({seconds: 3661}).minutes", "61"),
        // weekday_from_i is total and unchanged at the i64 extremes.
        (
            "RETURN date({year: 2024, week: 1, dayOfWeek: 9223372036854775807})",
            "2024-01-07",
        ),
        (
            "RETURN date({year: 2024, week: 1, dayOfWeek: -9223372036854775808})",
            "2024-01-06",
        ),
    ];
    for &(query, expected) in controls {
        let result = conn
            .query(query)
            .unwrap_or_else(|e| panic!("{query} must succeed: {e}"));
        let value = &result.rows[0][0];
        let shown = value
            .as_temporal()
            .map(|t| t.iso_string())
            .or_else(|| value.as_int().map(|i| i.to_string()))
            .unwrap_or_else(|| panic!("expected temporal/int for {query}, got {value:?}"));
        assert_eq!(shown, expected, "for {query}");
    }
}

/// The historically panicking arms still parse their valid inputs.
#[test]
fn temporal_pattern_valid_inputs_still_parse() {
    let db = Database::open(":memory:").unwrap();
    let mut conn = db.connect().unwrap();
    let controls: &[(&str, &str)] = &[
        ("RETURN date('2015-07-21', 'yyyy-MM-dd a')", "2015-07-21"),
        (
            "RETURN datetime('2015-07-21', 'yyyy-MM-ddXXX')",
            "2015-07-21T00:00Z",
        ),
        (
            "RETURN datetime('15-07-21', 'pppyy-MM-dd')",
            "2015-07-21T00:00Z",
        ),
        (
            r#"RETURN datetime("2015, 202 'day of year' 3 'day-of-week'", "yyyy, DDD 'day of year' c 'day-of-week'")"#,
            "2015-07-21T00:00Z",
        ),
    ];
    for &(query, expected) in controls {
        let result = conn.query(query).unwrap();
        let value = &result.rows[0][0];
        let temporal = value
            .as_temporal()
            .unwrap_or_else(|| panic!("expected temporal value for {query}, got {value:?}"));
        assert_eq!(temporal.iso_string(), expected, "for {query}");
    }
}
