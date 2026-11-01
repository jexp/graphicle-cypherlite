use std::collections::BTreeMap;
use std::panic::catch_unwind;

use super::construct::date_from_week;
use super::parse::parse_duration_iso;
use super::*;

#[test]
fn week_date() {
    let d = date_from_week(1816, 1, Weekday::Mon).unwrap();
    assert_eq!(d.format("%Y-%m-%d").to_string(), "1816-01-01");
}

#[test]
fn week_year_and_weekday_accessors() {
    let d = Temporal::Date(NaiveDate::from_ymd_opt(1984, 1, 1).unwrap());
    assert_eq!(d.component("weekYear"), Ok(Some(Value::Int(1983))));
    assert_eq!(d.component("week"), Ok(Some(Value::Int(52))));
    assert_eq!(d.component("weekDay"), Ok(Some(Value::Int(7))));
    assert_eq!(d.component("dayOfWeek"), Ok(Some(Value::Int(7))));
}

/// The direct constructor rejects `null` like its siblings; the executor's
/// `temporal_construct` short-circuits a null argument to `null` before any
/// constructor runs, so `date(null)` stays null at the Cypher level.
#[test]
fn date_null_rejects_like_its_siblings() {
    assert_eq!(
        construct_date(Some(&Value::Null)),
        Err(Error::invalid_argument_type("date()"))
    );
    assert_eq!(
        construct_datetime(Some(&Value::Null)),
        Err(Error::invalid_argument_type("datetime()"))
    );
}

/// chrono represents a leap second as second 59 with a fraction >= 1e9; the
/// truncation quantum must not wrap that out-of-day fraction to midnight.
#[test]
fn truncate_time_preserves_leap_second_encoding() {
    let leap = Temporal::Time {
        time: NaiveTime::from_hms_nano_opt(23, 59, 59, 1_234_567_890).unwrap(),
        offset: 0,
    };
    // Second-or-coarser units keep the represented second, no leap marker.
    let expected = |h, m, s| Temporal::Time {
        time: NaiveTime::from_hms_opt(h, m, s).unwrap(),
        offset: 0,
    };
    assert_eq!(
        truncate("time", "second", &leap, None).unwrap(),
        expected(23, 59, 59)
    );
    assert_eq!(
        truncate("time", "minute", &leap, None).unwrap(),
        expected(23, 59, 0)
    );
    assert_eq!(
        truncate("time", "hour", &leap, None).unwrap(),
        expected(23, 0, 0)
    );
    assert_eq!(
        truncate("time", "day", &leap, None).unwrap(),
        expected(0, 0, 0)
    );
    // Sub-second units truncate the fraction and keep the leap marker.
    assert_eq!(
        truncate("time", "millisecond", &leap, None).unwrap(),
        Temporal::Time {
            time: NaiveTime::from_hms_nano_opt(23, 59, 59, 1_234_000_000).unwrap(),
            offset: 0,
        }
    );
    assert_eq!(truncate("time", "nanosecond", &leap, None).unwrap(), leap);
}

#[test]
fn duration_component_table() {
    let d = construct_duration(Some(&Value::Map(
        [
            ("years".into(), Value::Int(1)),
            ("months".into(), Value::Int(4)),
            ("days".into(), Value::Int(10)),
            ("hours".into(), Value::Int(1)),
            ("minutes".into(), Value::Int(1)),
            ("seconds".into(), Value::Int(1)),
            ("nanoseconds".into(), Value::Int(111111111)),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into(),
    )))
    .unwrap();
    assert_eq!(d.component("years"), Ok(Some(Value::Int(1))));
    assert_eq!(d.component("quarters"), Ok(Some(Value::Int(5))));
    assert_eq!(d.component("months"), Ok(Some(Value::Int(16))));
    assert_eq!(d.component("weeks"), Ok(Some(Value::Int(1))));
    assert_eq!(d.component("minutes"), Ok(Some(Value::Int(61))));
    assert_eq!(d.component("seconds"), Ok(Some(Value::Int(3661))));
    assert_eq!(d.component("quartersOfYear"), Ok(Some(Value::Int(1))));
    assert_eq!(d.component("monthsOfQuarter"), Ok(Some(Value::Int(1))));
    assert_eq!(d.component("monthsOfYear"), Ok(Some(Value::Int(4))));
    assert_eq!(d.component("daysOfWeek"), Ok(Some(Value::Int(3))));
    assert_eq!(
        d.component("millisecondsOfSecond"),
        Ok(Some(Value::Int(111)))
    );
    assert_eq!(
        d.component("nanosecondsOfSecond"),
        Ok(Some(Value::Int(111111111)))
    );
}

#[test]
fn datetime_timezone_and_epoch_accessors() {
    let d = construct_datetime(Some(&Value::Map(
        [
            ("year".into(), Value::Int(1984)),
            ("month".into(), Value::Int(11)),
            ("day".into(), Value::Int(11)),
            ("hour".into(), Value::Int(12)),
            ("minute".into(), Value::Int(31)),
            ("second".into(), Value::Int(14)),
            ("nanosecond".into(), Value::Int(645876123)),
            ("timezone".into(), Value::String("Europe/Stockholm".into())),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into(),
    )))
    .unwrap();
    assert_eq!(
        d.component("timezone"),
        Ok(Some(Value::String("Europe/Stockholm".into())))
    );
    assert_eq!(
        d.component("offset"),
        Ok(Some(Value::String("+01:00".into())))
    );
    assert_eq!(d.component("epochSeconds"), Ok(Some(Value::Int(469020674))));
    assert_eq!(
        d.component("epochMillis"),
        Ok(Some(Value::Int(469020674645)))
    );
}

/// The exported resolver's three outcomes (ADR-0178). A unique wall time
/// resolves; a DST gap and an unknown zone name are the two typed errors; a
/// DST overlap takes the earliest (pre-transition) occurrence.
#[test]
fn zone_offset_at_resolves_unique_wall_time() {
    let local = NaiveDate::from_ymd_opt(1970, 1, 1)
        .unwrap()
        .and_hms_opt(2, 15, 0)
        .unwrap();
    assert_eq!(zone_offset_at("Europe/Paris", local), Ok(3600));
}

#[test]
fn zone_offset_at_unknown_zone_is_typed_error() {
    let local = NaiveDate::from_ymd_opt(2021, 3, 28)
        .unwrap()
        .and_hms_opt(2, 30, 0)
        .unwrap();
    assert_eq!(
        zone_offset_at("Not/AZone", local),
        Err(ZoneError::UnknownZone)
    );
}

#[test]
fn zone_offset_at_dst_gap_is_typed_error() {
    let local = NaiveDate::from_ymd_opt(2021, 3, 28)
        .unwrap()
        .and_hms_opt(2, 30, 0)
        .unwrap();
    assert_eq!(
        zone_offset_at("Europe/Stockholm", local),
        Err(ZoneError::NonexistentWallTime)
    );
}

#[test]
fn zone_offset_at_dst_overlap_takes_earliest_offset() {
    let local = NaiveDate::from_ymd_opt(2021, 10, 31)
        .unwrap()
        .and_hms_opt(2, 30, 0)
        .unwrap();
    assert_eq!(zone_offset_at("Europe/Stockholm", local), Ok(7200));
}

/// The construction path delegates to the exported resolver and maps its two
/// error variants back to the existing engine messages (ADR-0178).
#[test]
fn construct_datetime_iana_errors_keep_their_messages() {
    let fields = |timezone: &str| {
        Value::Map(
            [
                ("year".into(), Value::Int(2021)),
                ("month".into(), Value::Int(3)),
                ("day".into(), Value::Int(28)),
                ("hour".into(), Value::Int(2)),
                ("minute".into(), Value::Int(30)),
                ("timezone".into(), Value::String(timezone.into())),
            ]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into(),
        )
    };
    assert_eq!(
        construct_datetime(Some(&fields("Europe/Stockholm"))),
        Err(Error::ty("ambiguous datetime"))
    );
    assert_eq!(
        construct_datetime(Some(&fields("Not/AZone"))),
        Err(Error::ty("unknown timezone 'Not/AZone'"))
    );
}

/// F-1-27: a `timezone` map field that is not a string is a typed error, not
/// a silent UTC default; the field's absence stays the documented UTC
/// default.
#[test]
fn datetime_map_timezone_must_be_a_string() {
    let fields = |timezone: Option<Value>| {
        let mut m: BTreeMap<String, Value> = [
            ("year".into(), Value::Int(1984)),
            ("month".into(), Value::Int(11)),
            ("day".into(), Value::Int(11)),
            ("hour".into(), Value::Int(12)),
        ]
        .into_iter()
        .collect();
        if let Some(timezone) = timezone {
            m.insert("timezone".into(), timezone);
        }
        Value::Map(m.into())
    };
    assert_eq!(
        construct_datetime(Some(&fields(Some(Value::Int(42))))),
        Err(Error::invalid_argument_type("timezone must be STRING"))
    );
    let d = construct_datetime(Some(&fields(None))).unwrap();
    assert_eq!(d.component("timezone"), Ok(Some(Value::String("Z".into()))));
    assert_eq!(
        d.component("offset"),
        Ok(Some(Value::String("+00:00".into())))
    );
}

/// F-1-32: a `time` truncate's `timezone` is resolved at the statement's
/// pinned UTC instant, read as a UTC instant (`IanaAt::Utc`), not as a UTC
/// wall clock re-read in the zone. 2021-10-31T01:30Z is transition-adjacent
/// for Stockholm: the UTC reading is +01:00, while reading the same wall
/// clock locally would land before the fall-back and take +02:00.
#[test]
fn time_truncate_resolves_timezone_at_the_pinned_utc_instant() {
    let utc = NaiveDate::from_ymd_opt(2021, 10, 31)
        .unwrap()
        .and_hms_opt(1, 30, 0)
        .unwrap();
    let _clock = QueryClockGuard::pinned_at(utc);
    let extra = Value::Map(
        [("timezone".into(), Value::String("Europe/Stockholm".into()))]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into(),
    );
    let value = Temporal::Time {
        time: NaiveTime::from_hms_opt(12, 31, 14).unwrap(),
        offset: 0,
    };
    let out = truncate("time", "hour", &value, Some(&extra)).unwrap();
    assert_eq!(out.iso_string(), "12:00+01:00");
}

#[test]
fn datetime_tostring_roundtrip_equals() {
    let d = construct_datetime(Some(&Value::Map(
        [
            ("year".into(), Value::Int(1984)),
            ("month".into(), Value::Int(10)),
            ("day".into(), Value::Int(11)),
            ("hour".into(), Value::Int(12)),
            ("minute".into(), Value::Int(31)),
            ("second".into(), Value::Int(14)),
            ("nanosecond".into(), Value::Int(645876123)),
            ("timezone".into(), Value::String("+01:00".into())),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into(),
    )))
    .unwrap();
    assert_eq!(d.iso_string(), "1984-10-11T12:31:14.645876123+01:00");
    let parsed = parse_iso("datetime", &d.iso_string()).unwrap();
    assert_eq!(d.compare(&parsed), Some(std::cmp::Ordering::Equal));
    assert_eq!(
        format_pattern(&d, "yyyy-MM-dd'T'HH:mm:ss.SSSZ").unwrap(),
        "1984-10-11T12:31:14.645+0100"
    );
}

#[test]
fn parse_iso_year_month_and_compact_time_offset() {
    assert_eq!(
        parse_iso("date", "2015-07").unwrap().iso_string(),
        "2015-07-01"
    );
    assert_eq!(
        parse_iso("time", "2140-00:00").unwrap().iso_string(),
        "21:40Z"
    );
}

/// The value layer's two renderings: `full_seconds_string` (the
/// `toString` policy, ADR-0448) always emits the seconds field — Cypher
/// prints `toString(localtime('10:00'))` as `'10:00:00'`, never
/// `'10:00'` — while `iso_string` keeps the truncated whole-minute form.
#[test]
fn full_seconds_string_always_emits_the_seconds_field() {
    let cases = [
        ("localtime", "10:00", "10:00:00"),
        ("localtime", "10:00:30", "10:00:30"),
        ("localtime", "10:00:30.250", "10:00:30.25"),
        ("time", "10:00", "10:00:00Z"),
        ("time", "10:00+02:00", "10:00:00+02:00"),
        ("localdatetime", "2020-01-01T10:00", "2020-01-01T10:00:00"),
        ("datetime", "2020-01-01T10:00Z", "2020-01-01T10:00:00Z"),
        (
            "datetime",
            "2020-01-01T10:00+02:00",
            "2020-01-01T10:00:00+02:00",
        ),
    ];
    for (kind, input, expected) in cases {
        let value = parse_iso(kind, input).unwrap();
        assert_eq!(
            value.full_seconds_string(),
            expected,
            "for {kind}('{input}')"
        );
    }
    // The ISO rendering keeps its truncation of a whole minute.
    assert_eq!(
        parse_iso("localtime", "10:00").unwrap().iso_string(),
        "10:00"
    );
    assert_eq!(
        parse_iso("localdatetime", "2020-01-01T10:00")
            .unwrap()
            .iso_string(),
        "2020-01-01T10:00"
    );
}

#[test]
fn duration_iso_roundtrip_hours() {
    let t = parse_duration_iso("PT22H").unwrap();
    assert_eq!(t.iso_string(), "PT22H");
}

#[test]
fn truncate_week_day_of_week() {
    let d = construct_date(Some(&Value::Map(
        [
            ("year".into(), Value::Int(1984)),
            ("month".into(), Value::Int(10)),
            ("day".into(), Value::Int(11)),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into(),
    )))
    .unwrap();
    let extra = Value::Map(
        [("dayOfWeek".into(), Value::Int(2))]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into(),
    );
    let out = truncate("date", "week", &d, Some(&extra)).unwrap();
    assert_eq!(out.iso_string(), "1984-10-09");
}

#[test]
fn parse_fractional_and_datetime_style_duration() {
    assert_eq!(
        parse_duration_iso("P5M1.5D").unwrap().iso_string(),
        "P5M1DT12H"
    );
    assert_eq!(
        parse_duration_iso("P2.5W").unwrap().iso_string(),
        "P17DT12H"
    );
    assert_eq!(
        parse_duration_iso("P2012-02-02T14:37:21.545")
            .unwrap()
            .iso_string(),
        "P2012Y2M2DT14H37M21.545S"
    );
    assert_eq!(
        parse_duration_iso("P12Y5M-14DT16H").unwrap().iso_string(),
        "P12Y5M-14DT16H"
    );
}

/// Every `parse_pattern` arm must return `Err` on malformed input instead of
/// panicking. Covers the four historically verified panic inputs plus one
/// malformed variant per remaining arm (memdb-merge/03-temporal-parse-safety).
#[test]
fn parse_pattern_never_panics_on_malformed_input() {
    let cases: &[(&str, &str, &str)] = &[
        // Verified panic inputs.
        ("datetime", "abc", "yyyy-MM-ddXXX"),
        ("date", "abc", "yyyy-MM-dd a"),
        ("datetime", "abc", "pppyy-MM-dd"),
        ("datetime", "abc", "yyyy, DDD 'day of year' c 'day-of-week'"),
        // Remaining arms, one malformed variant each.
        ("duration", "abc", "yyyy-MM-dd"),
        ("date", "2015-01-01", "JJ"),
        ("datetime", "2015-01", "yyyy-MM"),
        ("localtime", "abc def", "H G"),
        ("localdatetime", "abc", "yyyy-MM-dd VV"),
        ("date", "abc", "yyyy-MM-dd EEEE"),
        // Civil (out-of-range) date hits the former `unreachable!()`.
        ("date", "2015-17-21", "yyyy-MM-dd EEEE"),
        ("datetime", "abc", "EEEE, MMMM d, G uuuu"),
        ("datetime", "abc", "yyyy-MM-dd k:mm z"),
        ("datetime", "abc", "yyyy-MM-dd K:mm a O"),
        ("datetime", "abc", "yyyy LLL d, A m s"),
        ("datetime", "abc", "'Nano:' n 'of day:' N yyyy-MM-dd"),
        ("datetime", "abc", "yyyy-MM-dd hh:mm a VV B"),
        ("datetime", "abc", "YYYY-'W'ww-e"),
        ("datetime", "abc", "QQQQ yyyy 'day': D"),
        // Compact arms: wrong length, and a multi-byte value whose byte
        // length matches the guard but whose slices are not char boundaries.
        ("date", "201501a", "yyyyMMdd"),
        ("date", "201\u{e5}071", "yyyyMMdd"),
        ("localtime", "12345678", "HHmmssSSS"),
        ("localtime", "1234x", "HHmmss"),
        ("localdatetime", "201501011210", "yyyyMMddHHmmss"),
        ("datetime", "2015010112101x", "yyyyMMddHHmmssZ"),
        ("time", "99xx99+01", "HHmmssZ"),
        // parse_iso fallback paths reachable from parse_pattern.
        ("time", "\u{e5}", "HH"),
        ("date", "2015W5\u{e5}5", "yyyy"),
        ("date", "not-a-date", "yyyy"),
    ];
    for &(kind, value, pattern) in cases {
        let outcome = catch_unwind(|| parse_pattern(kind, value, pattern))
            .unwrap_or_else(|_| panic!("parse_pattern({kind:?}, {value:?}, {pattern:?}) panicked"));
        assert!(
            outcome.is_err(),
            "parse_pattern({kind:?}, {value:?}, {pattern:?}) must fail, got {outcome:?}"
        );
    }
}

/// `timestamp()` must read the pinned statement instant, not the wall clock:
/// it agrees with `datetime().epochMillis` and does not move while the guard
/// is held.
#[test]
fn now_epoch_millis_matches_pinned_datetime_and_stays_fixed() {
    let _clock = QueryClockGuard::new();
    let epoch_millis = now_dt()
        .component("epochMillis")
        .unwrap()
        .expect("datetime() exposes epochMillis");
    let Value::Int(expected) = epoch_millis else {
        panic!("epochMillis must be an INTEGER, got {epoch_millis:?}");
    };
    assert_eq!(now_epoch_millis(), expected);
    // Long enough for any wall-clock read to have crossed a millisecond.
    std::thread::sleep(std::time::Duration::from_millis(10));
    assert_eq!(now_epoch_millis(), expected);
}

/// The historically panicking arms still parse their valid inputs.
#[test]
fn parse_pattern_valid_inputs_still_parse() {
    assert!(parse_pattern("datetime", "2015-07-21", "yyyy-MM-ddXXX").is_ok());
    assert!(parse_pattern("date", "2015-07-21", "yyyy-MM-dd a").is_ok());
    assert!(parse_pattern("datetime", "15-07-21", "pppyy-MM-dd").is_ok());
    assert!(parse_pattern(
        "datetime",
        "2015, 202 'day of year' 3 'day-of-week'",
        "yyyy, DDD 'day of year' c 'day-of-week'",
    )
    .is_ok());
}

/// LABS-261: `epochMillis`/`epochSeconds` map fields construct the temporal
/// value from the instant they name. Reference instant: 469020674645 ms =
/// 1984-11-11T11:31:14.645Z (the same one the accessor test pins, so
/// constructor and accessor round-trip each other).
#[test]
fn datetime_from_epoch_millis_and_epoch_seconds_map_fields() {
    let by_millis = construct_datetime(Some(&Value::Map(
        [("epochMillis".into(), Value::Int(469020674645))]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into(),
    )))
    .unwrap();
    // Compare on the ISO rendering: the constructor records the UTC zone
    // spelling explicitly, `parse_iso` leaves it unset, but both render "Z".
    assert_eq!(by_millis.iso_string(), "1984-11-11T11:31:14.645Z");
    assert_eq!(
        by_millis.component("epochMillis").unwrap(),
        Some(Value::Int(469020674645))
    );

    let by_seconds = construct_datetime(Some(&Value::Map(
        [("epochSeconds".into(), Value::Int(469020674))]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into(),
    )))
    .unwrap();
    assert_eq!(by_seconds.iso_string(), "1984-11-11T11:31:14Z");
}

/// An explicit `timezone` converts the named instant into that zone.
#[test]
fn datetime_from_epoch_millis_with_timezone() {
    let dt = construct_datetime(Some(&Value::Map(
        [
            ("epochMillis".into(), Value::Int(469020674645)),
            ("timezone".into(), Value::String("Europe/Stockholm".into())),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>()
        .into(),
    )))
    .unwrap();
    assert_eq!(
        dt.component("offset"),
        Ok(Some(Value::String("+01:00".into())))
    );
    assert_eq!(
        dt.component("epochMillis"),
        Ok(Some(Value::Int(469020674645)))
    );
}

/// `date`/`localdatetime`/`time` accept the same epoch fields, read at UTC.
#[test]
fn date_localdatetime_time_from_epoch_millis() {
    let ms = Value::Map(
        [("epochMillis".into(), Value::Int(469020674645))]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into(),
    );
    let d = construct_date(Some(&ms)).unwrap();
    assert_eq!(d, parse_iso("date", "1984-11-11").unwrap());

    let ldt = construct_localdatetime(Some(&ms)).unwrap();
    assert_eq!(
        ldt,
        parse_iso("localdatetime", "1984-11-11T11:31:14.645").unwrap()
    );

    let t = construct_time(Some(&ms)).unwrap();
    let Temporal::Time { time, offset } = t else {
        panic!("time({{epochMillis}}) must construct a time, got {t:?}");
    };
    assert_eq!(offset, 0);
    assert_eq!((time.hour(), time.minute(), time.second()), (11, 31, 14));
}

/// Epoch values outside chrono's timestamp range fail, not panic.
#[test]
fn epoch_fields_out_of_range_error() {
    let ms = Value::Map(
        [("epochMillis".into(), Value::Int(i64::MAX))]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into(),
    );
    assert!(construct_datetime(Some(&ms)).is_err());
    assert!(construct_date(Some(&ms)).is_err());
}

/// Non-integer epoch values are a typed error, not a silent default.
#[test]
fn epoch_fields_must_be_integers() {
    let ms = Value::Map(
        [("epochMillis".into(), Value::Float(469020674.645))]
            .into_iter()
            .collect::<BTreeMap<_, _>>()
            .into(),
    );
    assert!(construct_datetime(Some(&ms)).is_err());
}

// ADR-0325/0329: the typed helpers the Bolt structures rest on. The
// UTC-instant resolver's own cases live in `zone.rs` (ADR-0340).
#[test]
fn zone_region_classifies_iana_names_only() {
    assert_eq!(
        zone_region(Some("Europe/Stockholm")),
        Some("Europe/Stockholm")
    );
    for offset_only in [None, Some("Z"), Some("+01:00"), Some("UTC")] {
        assert_eq!(zone_region(offset_only), None, "{offset_only:?}");
    }
}

/// An explicit offset beside a bracketed zone disambiguates a DST fall-back
/// overlap, so the two halves of the repeated wall clock are both
/// representable — equal under `Temporal::equals`, ordered by
/// `Temporal::compare` (TCK TemporalCompare).
#[test]
fn zoned_literal_offset_disambiguates_a_dst_overlap() {
    let first = parse_iso("datetime", "2020-10-25T02:30+02:00[Europe/Stockholm]").unwrap();
    let second = parse_iso("datetime", "2020-10-25T02:30+01:00[Europe/Stockholm]").unwrap();
    // The offset beside the zone is preserved exactly as the literal
    // supplied it; the whole-minute wall clock keeps the ISO truncation,
    // while the `toString` rendering carries the seconds (ADR-0448).
    assert_eq!(
        first.iso_string(),
        "2020-10-25T02:30+02:00[Europe/Stockholm]"
    );
    assert_eq!(
        second.iso_string(),
        "2020-10-25T02:30+01:00[Europe/Stockholm]"
    );
    assert_eq!(
        first.full_seconds_string(),
        "2020-10-25T02:30:00+02:00[Europe/Stockholm]"
    );
    assert!(first.equals(&second));
    assert_eq!(first.compare(&second), Some(Ordering::Less));
    // A still-ambiguous wall clock (no offset) stays an error, as does a DST
    // gap.
    assert_eq!(
        parse_iso("datetime", "2020-10-25T02:30[Europe/Stockholm]"),
        Err(Error::ty("ambiguous datetime"))
    );
    assert_eq!(
        parse_iso("datetime", "2021-03-28T02:30[Europe/Stockholm]"),
        Err(Error::ty("ambiguous datetime"))
    );
}

#[test]
fn zoned_equality_degrades_not_panics_on_out_of_range_offset() {
    // The Bolt parameter decoders accept any i32 offset, so utc + offset can
    // leave chrono's range; equality (and every bucket derived from its
    // identity) degrades to the UTC wall clock instead of panicking.
    let extreme = |zone: Option<&str>| Temporal::DateTime {
        utc: chrono::NaiveDateTime::MAX,
        offset: 100_000,
        zone: zone.map(str::to_string),
    };
    let payload = extreme(None);
    let twin = payload.clone();
    assert!(payload.equals(&twin));
    assert_eq!(payload.datetime_eq_identity(), twin.datetime_eq_identity());
    // Distinct zones stay distinct even on the degraded local clock.
    assert!(!payload.equals(&extreme(Some("Europe/Stockholm"))));
    // Rendering the same payload takes the same stated UTC fallback: the
    // wall clock is the UTC value unchanged and the suffix is the supplied
    // offset (+27:46:40), so a regression that rendered another clock or
    // suffix without panicking still fails here.
    assert_eq!(
        payload.iso_string(),
        format!(
            "{}+27:46:40",
            chrono::NaiveDateTime::MAX.format("%Y-%m-%dT%H:%M:%S%.9f")
        )
    );
}

// Temporal `=` is offset/zone-sensitive and is not `compare() == Equal`.
#[test]
fn temporal_equality_is_offset_and_zone_sensitive() {
    use crate::temporal::ZoneId;
    use chrono::NaiveTime;

    let time = |h, m, offset| Temporal::Time {
        time: NaiveTime::from_hms_opt(h, m, 0).expect("valid time"),
        offset,
    };
    // Same instant, different offsets: unequal, and ordered on the offset.
    let a = time(12, 0, 3600);
    let b = time(11, 0, 0);
    assert!(!a.equals(&b));
    assert_eq!(a.compare(&b), Some(Ordering::Greater));
    // `Z` and `+00:00` are the same time.
    assert!(time(12, 0, 0).equals(&time(12, 0, 0)));

    // The two halves of a DST fall-back overlap: equal, yet ordered.
    let utc = chrono::NaiveDate::from_ymd_opt(2025, 10, 26)
        .expect("valid date")
        .and_hms_opt(0, 30, 0)
        .expect("valid time");
    let first = Temporal::DateTime {
        utc,
        offset: 7200,
        zone: Some("Europe/Stockholm".into()),
    };
    let second = Temporal::DateTime {
        utc: utc + chrono::Duration::hours(1),
        offset: 3600,
        zone: Some("Europe/Stockholm".into()),
    };
    assert!(first.equals(&second));
    assert_eq!(first.compare(&second), Some(Ordering::Less));
    // A named zone never equals the offset it denotes.
    let plain = Temporal::DateTime {
        utc,
        offset: 3600,
        zone: None,
    };
    assert!(!plain.equals(&second));
    // Durations are equality-only.
    let d = Temporal::Duration {
        months: 0,
        days: 1,
        nanos: 0,
    };
    assert!(d.equals(&d));
    assert_eq!(d.compare(&d), None);
    assert_eq!(ZoneId::of(Some("Z"), 0), ZoneId::Offset(0));
}
