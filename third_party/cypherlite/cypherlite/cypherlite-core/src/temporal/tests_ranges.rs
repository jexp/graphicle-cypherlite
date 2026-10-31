use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

use super::*;

/// One out-of-range case: a label plus the thunk exercising an arithmetic
/// path that must return `Err`, never unwind. The alias keeps the `Vec`
/// binding in the test below readable (clippy::type_complexity); the
/// lifetime parameter lets thunks borrow the test's local fixtures.
type OutOfRangeCase<'a> = (
    &'static str,
    Box<dyn FnOnce() -> std::result::Result<Temporal, Error> + 'a>,
);

/// Out-of-range temporal arithmetic must surface as `Err`, never unwind
/// (memdb-merge/03-temporal-parse-safety repair). Covers the three reviewer-
/// reproduced panics (`add_months` unwrap on out-of-chrono-range year,
/// `ChronoDur` overflow in `add_duration`, i64 overflow in the civil branch of
/// `duration_between_unit`) plus the sign/variant siblings found in the
/// whole-module re-sweep. Every case here panicked in debug before the repair.
#[test]
fn temporal_arith_out_of_range_errors_not_panics() {
    let date = Temporal::Date(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());
    let civil_far = Temporal::CivilDate {
        year: 9_000_000_000_000_000_000,
        month: 1,
        day: 1,
    };
    let civil_near = Temporal::CivilDate {
        year: -9_000_000_000_000_000_000,
        month: 1,
        day: 1,
    };
    let cases: Vec<OutOfRangeCase<'_>> = vec![
        // date + duration({months: 1e12}): add_months out-of-range year, both signs.
        (
            "add_months huge positive",
            Box::new(|| {
                add_duration(
                    &date,
                    &Temporal::Duration {
                        months: 1_000_000_000_000,
                        days: 0,
                        nanos: 0,
                    },
                )
            }),
        ),
        (
            "add_months huge negative",
            Box::new(|| {
                add_duration(
                    &date,
                    &Temporal::Duration {
                        months: -1_000_000_000_000,
                        days: 0,
                        nanos: 0,
                    },
                )
            }),
        ),
        (
            "sub_duration negation overflow",
            Box::new(|| {
                sub_duration(
                    &date,
                    &Temporal::Duration {
                        months: i64::MIN,
                        days: 0,
                        nanos: 0,
                    },
                )
            }),
        ),
        // datetime + duration({days: 1e18}): TimeDelta::days out of bounds.
        (
            "add_duration huge days on date",
            Box::new(|| {
                add_duration(
                    &date,
                    &Temporal::Duration {
                        months: 0,
                        days: i64::MAX / 10,
                        nanos: 0,
                    },
                )
            }),
        ),
        (
            "add_duration huge days on localdatetime",
            Box::new(|| {
                add_duration(
                    &Temporal::LocalDateTime(
                        NaiveDate::from_ymd_opt(2024, 1, 1)
                            .unwrap()
                            .and_time(NaiveTime::MIN),
                    ),
                    &Temporal::Duration {
                        months: 0,
                        days: i64::MAX / 10,
                        nanos: 0,
                    },
                )
            }),
        ),
        (
            "add_duration huge days on datetime",
            Box::new(|| {
                add_duration(
                    &Temporal::DateTime {
                        utc: NaiveDate::from_ymd_opt(2024, 1, 1)
                            .unwrap()
                            .and_time(NaiveTime::MIN),
                        offset: 0,
                        zone: Some("Z".into()),
                    },
                    &Temporal::Duration {
                        months: 0,
                        days: i64::MAX / 10,
                        nanos: 0,
                    },
                )
            }),
        ),
        (
            "add_duration nanos beyond i64",
            Box::new(|| {
                add_duration(
                    &Temporal::LocalDateTime(
                        NaiveDate::from_ymd_opt(2024, 1, 1)
                            .unwrap()
                            .and_time(NaiveTime::MIN),
                    ),
                    &Temporal::Duration {
                        months: 0,
                        days: 0,
                        nanos: i128::from(i64::MAX) + 1,
                    },
                )
            }),
        ),
        (
            "duration + duration months overflow",
            Box::new(|| {
                add_duration(
                    &Temporal::Duration {
                        months: i64::MAX,
                        days: 0,
                        nanos: 0,
                    },
                    &Temporal::Duration {
                        months: 1,
                        days: 0,
                        nanos: 0,
                    },
                )
            }),
        ),
        (
            "duration + duration nanos overflow",
            Box::new(|| {
                add_duration(
                    &Temporal::Duration {
                        months: 0,
                        days: 0,
                        nanos: i128::MAX,
                    },
                    &Temporal::Duration {
                        months: 0,
                        days: 0,
                        nanos: 1,
                    },
                )
            }),
        ),
        // duration.between on civil dates with ±9e18 years: i64 overflow in
        // `(yb - ya) * 12`; checked in i128 now, for every unit.
        (
            "duration.between civil huge years",
            Box::new(|| duration_between(&civil_near, &civil_far)),
        ),
        (
            "duration.between civil huge years reversed",
            Box::new(|| duration_between(&civil_far, &civil_near)),
        ),
        (
            "inmonths civil huge years",
            Box::new(|| duration_between_unit(&civil_near, &civil_far, BetweenUnit::Months)),
        ),
        (
            "indays civil huge years",
            Box::new(|| duration_between_unit(&civil_near, &civil_far, BetweenUnit::Days)),
        ),
        (
            "inseconds civil huge years",
            Box::new(|| duration_between_unit(&civil_near, &civil_far, BetweenUnit::Seconds)),
        ),
        // ISO duration accumulation overflow (months * 12, days +=, nanos +=).
        (
            "iso duration huge years",
            Box::new(|| parse_iso("duration", "P1000000000000000000000000Y")),
        ),
        (
            "iso duration huge repeated days",
            Box::new(|| parse_iso("duration", "P9000000000000000000D9000000000000000000D")),
        ),
        (
            "iso duration datetime-style huge year",
            Box::new(|| parse_iso("duration", "P9000000000000000000-01-01")),
        ),
        (
            "iso duration huge hours+minutes",
            Box::new(|| {
                parse_iso(
                    "duration",
                    "PT90000000000000000000000000H90000000000000000000000000M",
                )
            }),
        ),
        (
            "iso duration negation of min",
            Box::new(|| parse_iso("duration", "-P9000000000000000000Y")),
        ),
        // Timezone offset fields parsed into i32 arithmetic.
        (
            "time offset hour overflow",
            Box::new(|| parse_iso("time", "12:00+2147483647:00")),
        ),
        // Map-driven magnitude overflows in construction/truncation.
        (
            "duration map huge years",
            Box::new(|| {
                construct_duration(Some(&Value::Map(
                    [("years".into(), Value::Float(1e300))]
                        .into_iter()
                        .collect::<BTreeMap<_, _>>()
                        .into(),
                )))
            }),
        ),
        (
            "time map millisecond u32 overflow",
            Box::new(|| {
                construct_time(Some(&Value::Map(
                    [
                        ("hour".into(), Value::Int(1)),
                        ("millisecond".into(), Value::Int(5_000_000_000)),
                    ]
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into(),
                )))
            }),
        ),
        (
            "date map quarter u32 overflow",
            Box::new(|| {
                construct_date(Some(&Value::Map(
                    [
                        ("year".into(), Value::Int(2024)),
                        ("quarter".into(), Value::Int(2_000_000_000)),
                    ]
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into(),
                )))
            }),
        ),
        (
            "truncate dayOfWeek huge",
            Box::new(|| {
                truncate(
                    "date",
                    "week",
                    &date,
                    Some(&Value::Map(
                        [("dayOfWeek".into(), Value::Int(9_000_000_000_000_000_000))]
                            .into_iter()
                            .collect::<BTreeMap<_, _>>()
                            .into(),
                    )),
                )
            }),
        ),
        // Repair round 2: `NaiveDate - TimeDelta` panics when the Monday of
        // the week falls before NaiveDate::MIN. -262143-01-01 is a Thursday,
        // so Jan 1-4 all underflow; every kind routes through truncate_date.
        (
            "truncate week at chrono min (Thu)",
            Box::new(|| truncate("date", "week", &Temporal::Date(NaiveDate::MIN), None)),
        ),
        (
            "truncate week at chrono min (Fri)",
            Box::new(|| {
                truncate(
                    "date",
                    "week",
                    &Temporal::Date(NaiveDate::from_ymd_opt(-262143, 1, 2).unwrap()),
                    None,
                )
            }),
        ),
        (
            "truncate week at chrono min (Sat)",
            Box::new(|| {
                truncate(
                    "date",
                    "week",
                    &Temporal::Date(NaiveDate::from_ymd_opt(-262143, 1, 3).unwrap()),
                    None,
                )
            }),
        ),
        (
            "truncate week at chrono min (Sun)",
            Box::new(|| {
                truncate(
                    "date",
                    "week",
                    &Temporal::Date(NaiveDate::from_ymd_opt(-262143, 1, 4).unwrap()),
                    None,
                )
            }),
        ),
        (
            "truncate week at chrono min localdatetime",
            Box::new(|| {
                truncate(
                    "localdatetime",
                    "week",
                    &Temporal::LocalDateTime(NaiveDateTime::MIN),
                    None,
                )
            }),
        ),
        (
            "truncate week at chrono min datetime",
            Box::new(|| {
                truncate(
                    "datetime",
                    "week",
                    &Temporal::DateTime {
                        utc: NaiveDateTime::MIN,
                        offset: 0,
                        zone: Some("Z".into()),
                    },
                    None,
                )
            }),
        ),
        // weekyear sibling: ISO week-1 Monday of year -262143 precedes
        // NaiveDate::MIN; honest error via from_isoywd_opt, never a panic.
        (
            "truncate weekyear at chrono min",
            Box::new(|| truncate("date", "weekyear", &Temporal::Date(NaiveDate::MIN), None)),
        ),
        // Repair round 2 sweep find: chrono's from_isoywd_opt computes
        // `year ± 1` internally *before* its range guard, so i32-extreme
        // years (reachable via the truncating `as i32` map casts) panicked
        // inside chrono. The guarded date_from_week rejects them first.
        (
            "week date map i32-min year",
            Box::new(|| {
                construct_date(Some(&Value::Map(
                    [
                        ("year".into(), Value::Int(2_147_483_648)),
                        ("week".into(), Value::Int(1)),
                    ]
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into(),
                )))
            }),
        ),
        (
            "week date map i32-max year",
            Box::new(|| {
                construct_date(Some(&Value::Map(
                    [
                        ("year".into(), Value::Int(2_147_483_647)),
                        ("week".into(), Value::Int(52)),
                        ("dayOfWeek".into(), Value::Int(6)),
                    ]
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into(),
                )))
            }),
        ),
        (
            "week date string huge year",
            Box::new(|| parse_iso("date", "2147483647W526")),
        ),
        (
            "week date string negative huge year",
            Box::new(|| parse_iso("date", "-2147483648W011")),
        ),
        (
            "week date pattern huge year",
            Box::new(|| parse_pattern("datetime", "2147483647-W52-6", "YYYY-'W'ww-e")),
        ),
        // Repair round 2: the Date arm of add_duration truncated the i128
        // day-equivalent of nanos through `as i64`; beyond-i64 day counts
        // must error honestly instead of wrapping to a small day count.
        (
            "date + duration with beyond-i64 day-equivalent nanos",
            Box::new(|| {
                add_duration(
                    &date,
                    &Temporal::Duration {
                        months: 0,
                        days: 0,
                        nanos: i128::from(i64::MAX) * 86_400_000_000_000 * 2,
                    },
                )
            }),
        ),
        // Repair round 2 sweep finds in the negation/cast-of-extreme class.
        (
            "duration.between with i32::MIN time offset",
            Box::new(|| {
                let t = Temporal::Time {
                    time: NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
                    offset: i32::MIN,
                };
                let dt = parse_iso("datetime", "2015-01-01T00:00[Europe/Stockholm]").unwrap();
                duration_between(&t, &dt)
            }),
        ),
        (
            "duration.between with i32::MIN time offset reversed",
            Box::new(|| {
                let t = Temporal::Time {
                    time: NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
                    offset: i32::MIN,
                };
                let dt = parse_iso("datetime", "2015-01-01T00:00[Europe/Stockholm]").unwrap();
                duration_between(&dt, &t)
            }),
        ),
        // Civil years beyond i32 wrapped into chrono range through `as i32`.
        (
            "truncate day on civil year wrapping into range",
            Box::new(|| {
                truncate(
                    "date",
                    "day",
                    &Temporal::CivilDate {
                        year: 4_294_967_297,
                        month: 1,
                        day: 1,
                    },
                    None,
                )
            }),
        ),
        // Map fields beyond u32/i32 wrapped into range through `as` casts.
        (
            "date map year wraps into range",
            Box::new(|| {
                construct_date(Some(&Value::Map(
                    [("year".into(), Value::Int(4_294_967_297))]
                        .into_iter()
                        .collect::<BTreeMap<_, _>>()
                        .into(),
                )))
            }),
        ),
        (
            "date map month wraps into range",
            Box::new(|| {
                construct_date(Some(&Value::Map(
                    [
                        ("year".into(), Value::Int(2024)),
                        ("month".into(), Value::Int(4_294_967_297)),
                    ]
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into(),
                )))
            }),
        ),
        (
            "time map hour wraps into range",
            Box::new(|| {
                construct_time(Some(&Value::Map(
                    [("hour".into(), Value::Int(4_294_967_297))]
                        .into_iter()
                        .collect::<BTreeMap<_, _>>()
                        .into(),
                )))
            }),
        ),
        (
            "truncate extras day wraps into range",
            Box::new(|| {
                truncate(
                    "date",
                    "day",
                    &date,
                    Some(&Value::Map(
                        [("day".into(), Value::Int(4_294_967_297))]
                            .into_iter()
                            .collect::<BTreeMap<_, _>>()
                            .into(),
                    )),
                )
            }),
        ),
        (
            "datetime map offset underflow at chrono min",
            Box::new(|| {
                construct_datetime(Some(&Value::Map(
                    [
                        ("year".into(), Value::Int(-262143)),
                        ("month".into(), Value::Int(1)),
                        ("day".into(), Value::Int(1)),
                        ("timezone".into(), Value::String("+99:99".into())),
                    ]
                    .into_iter()
                    .collect::<BTreeMap<_, _>>()
                    .into(),
                )))
            }),
        ),
        (
            "pattern millis i64::MIN",
            Box::new(|| {
                parse_pattern(
                    "datetime",
                    "2015 Jan 1, -9223372036854775808 0 0",
                    "yyyy LLL d, A m s",
                )
            }),
        ),
        (
            "scale duration huge factor",
            Box::new(|| {
                scale_duration(
                    &Temporal::Duration {
                        months: 5,
                        days: 0,
                        nanos: 0,
                    },
                    1e300,
                )
            }),
        ),
    ];
    for (name, f) in cases {
        let outcome = catch_unwind(std::panic::AssertUnwindSafe(f))
            .unwrap_or_else(|_| panic!("{name} panicked"));
        assert!(outcome.is_err(), "{name} must fail, got {outcome:?}");
    }
}

/// Duration components derived from the i128 nanosecond count must error
/// honestly when the total exceeds i64, instead of truncating through `as`
/// casts into garbage `Value::Int`s (memdb-merge/03-temporal-parse-safety
/// repair round 2). The `*OfSecond`/`*OfHour` splits are < 2^63 by
/// construction and keep working at the same magnitudes.
#[test]
fn duration_component_out_of_range_errors_not_garbage() {
    // nanos = 1e37 (duration({seconds: 1e28})): totals exceed i64.
    let huge = Temporal::Duration {
        months: 0,
        days: 0,
        nanos: 10_000_000_000_000_000_000_000_000_000_000_000_000i128,
    };
    let huge_neg = Temporal::Duration {
        months: 0,
        days: 0,
        nanos: -10_000_000_000_000_000_000_000_000_000_000_000_000i128,
    };
    for k in [
        "hours",
        "minutes",
        "seconds",
        "nanoseconds",
        "milliseconds",
        "microseconds",
    ] {
        for d in [&huge, &huge_neg] {
            let outcome = catch_unwind(AssertUnwindSafe(|| d.component(k)))
                .unwrap_or_else(|_| panic!("component({k}) panicked"));
            assert!(
                outcome.is_err(),
                "component({k}) on {d:?} must fail, got {outcome:?}"
            );
        }
    }
    // In-range splits of the same value are unaffected.
    assert!(huge.component("minutesOfHour").unwrap().is_some());
    assert!(huge.component("secondsOfMinute").unwrap().is_some());
    assert!(huge.component("nanosecondsOfSecond").unwrap().is_some());
    assert!(huge.component("millisecondsOfSecond").unwrap().is_some());
    assert!(huge.component("microsecondsOfSecond").unwrap().is_some());
    // Months/days components never touch the i128 nanos.
    assert_eq!(huge.component("months"), Ok(Some(Value::Int(0))));
    // Unknown keys stay None.
    assert_eq!(huge.component("nope"), Ok(None));
}

/// Boundary values at the chrono range edges keep producing correct results
/// where the result is representable (repair round 2 positive controls).
#[test]
fn temporal_boundary_in_range_still_works() {
    // -262143-01-05 is the first Monday inside chrono range.
    let mon = Temporal::Date(NaiveDate::from_ymd_opt(-262143, 1, 5).unwrap());
    assert_eq!(truncate("date", "week", &mon, None).unwrap(), mon);
    let tue = Temporal::Date(NaiveDate::from_ymd_opt(-262143, 1, 6).unwrap());
    assert_eq!(truncate("date", "week", &tue, None).unwrap(), mon);
    // Units that never move before the input keep working at NaiveDate::MIN.
    let min = Temporal::Date(NaiveDate::MIN);
    for unit in [
        "millennium",
        "century",
        "decade",
        "year",
        "quarter",
        "month",
        "day",
    ] {
        assert!(
            truncate("date", unit, &min, None).is_ok(),
            "truncate({unit}) at NaiveDate::MIN must succeed"
        );
    }
    // Same at the upper bound for the units that only move backward.
    let max = Temporal::Date(NaiveDate::MAX);
    for unit in [
        "millennium",
        "century",
        "decade",
        "year",
        "weekyear",
        "quarter",
        "month",
        "week",
        "day",
    ] {
        assert!(
            truncate("date", unit, &max, None).is_ok(),
            "truncate({unit}) at NaiveDate::MAX must succeed"
        );
    }
    // duration.inSeconds across the full chrono span: previously a silent
    // PT0S via `num_nanoseconds().unwrap_or(0)`; now the exact i128 value.
    let span = duration_between_unit(
        &Temporal::LocalDateTime(NaiveDateTime::MIN),
        &Temporal::LocalDateTime(NaiveDateTime::MAX),
        BetweenUnit::Seconds,
    )
    .unwrap();
    let days = (NaiveDate::MAX - NaiveDate::MIN).num_days();
    let expected = i128::from(days) * 86_400_000_000_000 + 86_399_999_999_999;
    assert_eq!(
        span,
        Temporal::Duration {
            months: 0,
            days: 0,
            nanos: expected,
        }
    );
    assert_eq!(span.iso_string(), "PT4595796695H59M59.999999999S");
    // weekday_from_i stays total (and unchanged) at the i64 extremes:
    // i64::MAX -> Sunday, i64::MIN -> Saturday, exactly like the old
    // `(i - 1).rem_euclid(7) + 1` mapping that overflowed at i64::MIN.
    let week_map = |dow: i64| {
        construct_date(Some(&Value::Map(
            [
                ("year".into(), Value::Int(2024)),
                ("week".into(), Value::Int(1)),
                ("dayOfWeek".into(), Value::Int(dow)),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into(),
        )))
        .unwrap()
        .iso_string()
    };
    assert_eq!(week_map(i64::MAX), "2024-01-07");
    assert_eq!(week_map(i64::MIN), "2024-01-06");
    assert_eq!(week_map(1), "2024-01-01");
    assert_eq!(week_map(7), "2024-01-07");
    // 0 maps to Sunday, i.e. the Sunday of ISO week 1 (unchanged behavior).
    assert_eq!(week_map(0), "2024-01-07");
}

/// In-range arithmetic right at the chrono boundary and large-but-valid civil
/// dates keep working (no regression to legitimate results).
#[test]
fn temporal_arith_in_range_still_works() {
    let date = Temporal::Date(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());
    let plus_one_month = add_duration(
        &date,
        &Temporal::Duration {
            months: 1,
            days: 0,
            nanos: 0,
        },
    )
    .unwrap();
    assert_eq!(plus_one_month.iso_string(), "2024-02-01");
    // TCK-range civil dates (±999999999) still produce exact durations.
    let near = Temporal::CivilDate {
        year: -999_999_999,
        month: 1,
        day: 1,
    };
    let far = Temporal::CivilDate {
        year: 999_999_999,
        month: 1,
        day: 1,
    };
    let d = duration_between(&far, &near).unwrap();
    assert_eq!(
        d,
        Temporal::Duration {
            months: -23_999_999_976,
            days: 0,
            nanos: 0,
        }
    );
}
