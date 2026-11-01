//! Construction of temporal values from components, maps, and truncation.
//! The zone-resolution cluster lives in [`super::zone`] (ADR-0340); the
//! truncate cluster below is the next extraction when this file approaches
//! the file-size budget.

use chrono::{
    Datelike, Days, Duration as ChronoDur, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday,
};

use crate::{Error, Result, Value};

use super::parse::parse_iso;
use super::zone::{add_offset, apply_timezone, offset_from_map, sub_offset, IanaAt, ResolvedZone};
use super::{
    f64_to_i128_range, f64_to_i64_range, map_temporal, nanos_to_time, now_date, now_dt,
    now_local_dt, now_local_time, now_time, now_utc_dt, range_err, time_to_nanos, weekday_from_i,
    Temporal, AVG_DAYS_PER_MONTH, NANOS_PER_DAY,
};

fn map_i(map: &std::collections::BTreeMap<String, Value>, k: &str) -> Option<i64> {
    map.get(k).and_then(Value::as_int)
}

fn map_num(map: &std::collections::BTreeMap<String, Value>, k: &str) -> Option<f64> {
    map.get(k).and_then(Value::as_number)
}

/// `(epoch seconds, subsecond nanos)` named by the map's `epochMillis` /
/// `epochSeconds` fields (LABS-261). Both the camelCase and the lowercase
/// spelling are accepted, like the engine's other multi-spelling fields
/// (`dayOfWeek`/`dayofweek`). `None` when the map names no epoch field; if
/// both spellings appear, `epochMillis` wins because it is the more
/// precise one.
fn epoch_from_map(m: &std::collections::BTreeMap<String, Value>) -> Result<Option<(i64, i64)>> {
    // Probe the millis spelling FIRST (F-4-2): two direct lookups per
    // field, millis before seconds — the same double-lookup shape the
    // sibling multi-spelling fields use — so a map carrying both keeps
    // the documented precedence (a lexicographic key scan would hand the
    // win to seconds).
    if let Some(v) = m.get("epochMillis").or_else(|| m.get("epochmillis")) {
        let ms = v
            .as_int()
            .ok_or_else(|| Error::ty("epochMillis must be an integer"))?;
        return Ok(Some((ms.div_euclid(1000), ms.rem_euclid(1000) * 1_000_000)));
    }
    if let Some(v) = m.get("epochSeconds").or_else(|| m.get("epochseconds")) {
        let s = v
            .as_int()
            .ok_or_else(|| Error::ty("epochSeconds must be an integer"))?;
        return Ok(Some((s, 0)));
    }
    Ok(None)
}

/// The UTC naive datetime the map's epoch fields name, or `None` when the
/// map carries no epoch field. Epoch fields are instants on the UTC timeline
/// by definition, so the conversion itself needs no zone.
fn epoch_utc_dt(m: &std::collections::BTreeMap<String, Value>) -> Result<Option<NaiveDateTime>> {
    let Some((secs, nanos)) = epoch_from_map(m)? else {
        return Ok(None);
    };
    // `from_timestamp` takes subsecond nanos in `0..=999_999_999`;
    // `div_euclid`/`rem_euclid` already keep both parts sign-consistent,
    // so the nanos are always in range (F-3-2: the old clamp could never
    // fire).
    debug_assert!(nanos <= 999_999_999);
    chrono::DateTime::from_timestamp(secs, nanos as u32)
        .map(|d| d.naive_utc())
        .ok_or_else(|| Error::ty("epoch out of range"))
        .map(Some)
}

fn date_from_parts(year: i32, month: u32, day: u32) -> Result<NaiveDate> {
    NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| Error::ty("invalid date"))
}

pub(super) fn date_from_week(year: i32, week: u32, dow: Weekday) -> Result<NaiveDate> {
    // chrono's `from_isoywd_opt` computes `year ± 1` internally *before* its
    // range guard, so i32-extreme years (reachable here from week-date
    // strings, e.g. '2147483647W526') panic inside chrono. Restrict to
    // chrono's range ±1: the internal `year ± 1` then stays within i32 and
    // any genuinely out-of-range result still returns `None`.
    if !(-262_144..=262_144).contains(&year) {
        return Err(Error::ty("invalid week date"));
    }
    NaiveDate::from_isoywd_opt(year, week, dow).ok_or_else(|| Error::ty("invalid week date"))
}

pub fn construct_date(arg: Option<&Value>) -> Result<Temporal> {
    match arg {
        None => Ok(now_date()),
        Some(Value::String(s)) => parse_iso("date", s),
        Some(Value::Temporal(t)) => Ok(Temporal::Date(date_of(t)?)),
        Some(Value::Map(m)) => {
            // `date({epochMillis: n})` is the calendar date of that instant.
            // Timezone assumption: UTC — the engine's default zone reading for
            // epoch fields (Neo4j reads them in the session's default timezone,
            // which CypherLite pins to UTC).
            if let Some(utc) = epoch_utc_dt(m)? {
                return Ok(Temporal::Date(utc.date()));
            }
            Ok(Temporal::Date(date_from_map(m)?))
        }
        _ => Err(Error::invalid_argument_type("date()")),
    }
}

fn date_of(t: &Temporal) -> Result<NaiveDate> {
    match t {
        Temporal::Date(d) => Ok(*d),
        Temporal::CivilDate { year, month, day } => {
            // `as i32` would wrap extreme civil years into chrono range;
            // convert honestly so only representable years can succeed.
            let year =
                i32::try_from(*year).map_err(|_| Error::ty("civil date out of chrono range"))?;
            NaiveDate::from_ymd_opt(year, *month, *day)
                .ok_or_else(|| Error::ty("civil date out of chrono range"))
        }
        Temporal::LocalDateTime(dt) => Ok(dt.date()),
        Temporal::DateTime { utc, offset, .. } => Ok(add_offset(*utc, *offset)?.date()),
        _ => Err(Error::ty("cannot convert to date")),
    }
}

fn date_from_map(m: &std::collections::BTreeMap<String, Value>) -> Result<NaiveDate> {
    let mut year = 0i32;
    let mut month = 1u32;
    let mut day = 1u32;
    let mut dow_default = 1i64;
    if let Some(t) = map_temporal(m, "date").or_else(|| map_temporal(m, "datetime")) {
        let d = date_of(t)?;
        year = d.year();
        month = d.month();
        day = d.day();
        dow_default = d.weekday().number_from_monday() as i64;
        if m.contains_key("week") && !m.contains_key("year") {
            year = d.iso_week().year();
        }
    }
    if let Some(y) = map_i(m, "year") {
        year = i32::try_from(y).map_err(|_| Error::ty("invalid date"))?;
    }
    if let Some(w) = map_i(m, "week") {
        let dow = map_i(m, "dayOfWeek")
            .or_else(|| map_i(m, "dayofweek"))
            .unwrap_or(dow_default);
        let week = u32::try_from(w).map_err(|_| Error::ty("invalid week date"))?;
        return date_from_week(year, week, weekday_from_i(dow));
    }
    if let Some(od) = map_i(m, "ordinalDay").or_else(|| map_i(m, "ordinalday")) {
        let ordinal = u32::try_from(od).map_err(|_| Error::ty("invalid date"))?;
        return NaiveDate::from_yo_opt(year, ordinal).ok_or_else(|| Error::ty("invalid date"));
    }
    if let Some(q) = map_i(m, "quarter") {
        // Quarter-of-year math on a user-controlled i64: compute in i128 and
        // error on anything out of range instead of wrapping a u32.
        let quarter_month = |moq: u32| -> Result<u32> {
            let month = (i128::from(q) - 1) * 3 + i128::from(moq);
            u32::try_from(month).map_err(|_| Error::ty("invalid date"))
        };
        let moq = if m.contains_key("month")
            || m.contains_key("dayOfQuarter")
            || m.contains_key("dayofquarter")
        {
            1u32
        } else {
            (month.saturating_sub(1) % 3) + 1
        };
        month = quarter_month(moq)?.max(1);
        if let Some(dq) = map_i(m, "dayOfQuarter").or_else(|| map_i(m, "dayofquarter")) {
            let start = date_from_parts(year, quarter_month(1)?.max(1), 1)?;
            return start
                .checked_add_days(Days::new((dq as u64).saturating_sub(1)))
                .ok_or_else(|| Error::ty("invalid date"));
        }
    }
    if let Some(mo) = map_i(m, "month") {
        month = u32::try_from(mo).map_err(|_| Error::ty("invalid date"))?;
    }
    if let Some(d) = map_i(m, "day") {
        day = u32::try_from(d).map_err(|_| Error::ty("invalid date"))?;
    }
    date_from_parts(year, month, day)
}

/// `true` when the map carries explicit date fields (`year`/`date`/
/// `datetime`), i.e. the date comes from `date_from_map`.
fn has_explicit_date_fields(m: &std::collections::BTreeMap<String, Value>) -> bool {
    m.contains_key("date") || m.contains_key("datetime") || m.contains_key("year")
}

/// How a component map supplies the date part of a temporal constructor.
/// Classified once so constructors share one probe vocabulary.
enum DateContext<'a> {
    /// Explicit date fields: build with `date_from_map`.
    Explicit,
    /// No explicit fields, but a zoned DateTime under `time`/`datetime`
    /// supplies the date through its local wall clock; carries the proved
    /// fields so consumers do not re-match.
    Source { utc: &'a NaiveDateTime, offset: i32 },
    /// No date information at all: the epoch date default.
    None,
}

fn date_context(m: &std::collections::BTreeMap<String, Value>) -> DateContext<'_> {
    if has_explicit_date_fields(m) {
        return DateContext::Explicit;
    }
    if let Some(Temporal::DateTime { utc, offset, .. }) =
        map_temporal(m, "time").or_else(|| map_temporal(m, "datetime"))
    {
        return DateContext::Source {
            utc,
            offset: *offset,
        };
    }
    DateContext::None
}

fn time_from_map(m: &std::collections::BTreeMap<String, Value>) -> Result<NaiveTime> {
    let mut hour = 0u32;
    let mut minute = 0u32;
    let mut second = 0u32;
    let mut nano = 0u32;
    if let Some(t) = map_temporal(m, "time").or_else(|| map_temporal(m, "datetime")) {
        if let Some(tm) = time_of(t) {
            hour = tm.hour();
            minute = tm.minute();
            second = tm.second();
            nano = tm.nanosecond();
        }
    }
    if let Some(h) = map_i(m, "hour") {
        hour = u32::try_from(h).map_err(|_| Error::ty("invalid time"))?;
    }
    if let Some(mi) = map_i(m, "minute") {
        minute = u32::try_from(mi).map_err(|_| Error::ty("invalid time"))?;
    }
    if let Some(s) = map_i(m, "second") {
        second = u32::try_from(s).map_err(|_| Error::ty("invalid time"))?;
    }
    let nano = nano_with_subsecond_fields(nano, m)?;
    NaiveTime::from_hms_nano_opt(hour, minute, second, nano)
        .ok_or_else(|| Error::ty("invalid time"))
}

/// Sum the subsecond map fields (`millisecond`/`microsecond`/`nanosecond`)
/// into a nanosecond count. These are user-controlled i64 magnitudes, so
/// accumulate in i128 and error on overflow instead of wrapping u32.
fn nano_with_subsecond_fields(
    base: u32,
    m: &std::collections::BTreeMap<String, Value>,
) -> Result<u32> {
    let mut total = i128::from(base);
    if let Some(ms) = map_i(m, "millisecond") {
        total += i128::from(ms) * 1_000_000;
    }
    if let Some(us) = map_i(m, "microsecond") {
        total += i128::from(us) * 1_000;
    }
    if let Some(n) = map_i(m, "nanosecond") {
        total += i128::from(n);
    }
    u32::try_from(total).map_err(|_| Error::ty("invalid time"))
}

fn time_of(t: &Temporal) -> Option<NaiveTime> {
    match t {
        Temporal::LocalTime(t) | Temporal::Time { time: t, .. } => Some(*t),
        Temporal::LocalDateTime(dt) => Some(dt.time()),
        Temporal::DateTime { utc, offset, .. } => utc
            .checked_add_signed(ChronoDur::seconds(i64::from(*offset)))
            .map(|local| local.time()),
        _ => None,
    }
}

pub fn construct_localtime(arg: Option<&Value>) -> Result<Temporal> {
    match arg {
        None => Ok(now_local_time()),
        Some(Value::String(s)) => parse_iso("localtime", s),
        Some(Value::Temporal(t)) => time_of(t)
            .map(Temporal::LocalTime)
            .ok_or_else(|| Error::ty("cannot convert to localtime")),
        Some(Value::Map(m)) => {
            if !m.contains_key("hour") && !m.contains_key("time") && !m.contains_key("datetime") {
                return Err(Error::ty("invalid localtime field map"));
            }
            Ok(Temporal::LocalTime(time_from_map(m)?))
        }
        _ => Err(Error::invalid_argument_type("localtime()")),
    }
}

pub fn construct_time(arg: Option<&Value>) -> Result<Temporal> {
    match arg {
        None => Ok(now_time()),
        Some(Value::String(s)) => parse_iso("time", s),
        Some(Value::Temporal(Temporal::Time { time, offset })) => Ok(Temporal::Time {
            time: *time,
            offset: *offset,
        }),
        Some(Value::Temporal(t)) => {
            let time = time_of(t).ok_or_else(|| Error::ty("cannot convert to time"))?;
            let offset = match t {
                Temporal::DateTime { offset, .. } => *offset,
                _ => 0,
            };
            Ok(Temporal::Time { time, offset })
        }
        Some(Value::Map(m)) => {
            if let Some(utc) = epoch_utc_dt(m)? {
                // `time({epochMillis: n})` is the time of day of that instant.
                // Timezone assumption: without a `timezone` field the instant
                // is read at UTC (offset 0), like the datetime constructor.
                let resolved = offset_from_map(m, IanaAt::Utc(utc))?;
                return time_wall_clock(utc, resolved.offset);
            }
            let time = time_from_map(m)?;
            let epoch = || NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let date = match date_context(m) {
                DateContext::Explicit => date_from_map(m)?,
                DateContext::Source { utc, offset } => add_offset(*utc, offset)?.date(),
                _ => epoch(),
            };
            let (utc, resolved) = apply_timezone(date.and_time(time), m)?;
            time_wall_clock(utc, resolved.offset)
        }
        _ => Err(Error::invalid_argument_type("time()")),
    }
}

/// The wall-clock `Temporal::Time` of the UTC instant `utc` read at
/// `offset` — the shared tail of `construct_time`'s epoch and field-map
/// paths (F-1-20).
fn time_wall_clock(utc: NaiveDateTime, offset: i32) -> Result<Temporal> {
    let wall = add_offset(utc, offset)?;
    Ok(Temporal::Time {
        time: wall.time(),
        offset,
    })
}

pub fn construct_localdatetime(arg: Option<&Value>) -> Result<Temporal> {
    match arg {
        None => Ok(now_local_dt()),
        Some(Value::String(s)) => parse_iso("localdatetime", s),
        Some(Value::Temporal(t)) => Ok(Temporal::LocalDateTime(local_dt_of(t)?)),
        Some(Value::Map(m)) => {
            // `localdatetime({epochMillis: n})` is the naive UTC wall clock of
            // that instant (timezone assumption: UTC, since a localdatetime
            // carries no zone of its own).
            if let Some(utc) = epoch_utc_dt(m)? {
                return Ok(Temporal::LocalDateTime(utc));
            }
            if !has_explicit_date_fields(m) {
                return Err(Error::ty("invalid localdatetime field map"));
            }
            let d = date_from_map(m)?;
            let tm = time_from_map(m).unwrap_or_else(|_| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
            Ok(Temporal::LocalDateTime(d.and_time(tm)))
        }
        _ => Err(Error::invalid_argument_type("localdatetime()")),
    }
}

fn local_dt_of(t: &Temporal) -> Result<NaiveDateTime> {
    match t {
        Temporal::LocalDateTime(dt) => Ok(*dt),
        Temporal::Date(d) => Ok(d.and_time(NaiveTime::MIN)),
        Temporal::DateTime { utc, offset, .. } => add_offset(*utc, *offset),
        _ => Err(Error::ty("cannot convert to localdatetime")),
    }
}

pub fn construct_datetime(arg: Option<&Value>) -> Result<Temporal> {
    match arg {
        None => Ok(now_dt()),
        Some(Value::String(s)) => parse_iso("datetime", s),
        Some(Value::Temporal(t @ Temporal::DateTime { .. })) => Ok(t.clone()),
        Some(Value::Temporal(t)) => {
            let local = local_dt_of(t)?;
            Ok(Temporal::DateTime {
                utc: local,
                offset: 0,
                zone: Some("Z".into()),
            })
        }
        Some(Value::Map(m)) => {
            // `datetime({epochMillis: n})` yields a zoned datetime: the
            // instant read at UTC (zone `Z`, offset 0) unless an explicit
            // `timezone` field converts it into that zone.
            if let Some(utc) = epoch_utc_dt(m)? {
                let resolved = offset_from_map(m, IanaAt::Utc(utc))?;
                return Ok(Temporal::DateTime {
                    utc,
                    offset: resolved.offset,
                    zone: resolved.zone,
                });
            }
            let d = date_from_map(m)?;
            let tm = time_from_map(m).unwrap_or(NaiveTime::MIN);
            let local = d.and_time(tm);
            let (utc, resolved) = apply_timezone(local, m)?;
            Ok(Temporal::DateTime {
                utc,
                offset: resolved.offset,
                zone: resolved.zone,
            })
        }
        _ => Err(Error::invalid_argument_type("datetime()")),
    }
}

pub fn construct_duration(arg: Option<&Value>) -> Result<Temporal> {
    match arg {
        Some(Value::String(s)) => parse_iso("duration", s),
        Some(Value::Temporal(t @ Temporal::Duration { .. })) => Ok(t.clone()),
        Some(Value::Map(m)) => {
            // (field, scale) tables per tier; the trunc/fract cascade between
            // tiers carries fractional remainders downward.
            const MONTH_FIELDS: &[(&str, f64)] =
                &[("years", 12.0), ("months", 1.0), ("quarters", 3.0)];
            const DAY_FIELDS: &[(&str, f64)] = &[("weeks", 7.0), ("days", 1.0)];
            const NANO_FIELDS: &[(&str, f64)] = &[
                ("hours", 3_600_000_000_000.0),
                ("minutes", 60_000_000_000.0),
                ("seconds", 1_000_000_000.0),
                ("milliseconds", 1_000_000.0),
                ("microseconds", 1_000.0),
                ("nanoseconds", 1.0),
            ];
            let mut months = 0.0f64;
            let mut days = 0.0f64;
            let mut nanos = 0.0f64;
            for (k, scale) in MONTH_FIELDS {
                if let Some(v) = map_num(m, k) {
                    months += v * scale;
                }
            }
            let whole_months = months.trunc();
            days += months.fract() * AVG_DAYS_PER_MONTH;
            for (k, scale) in DAY_FIELDS {
                if let Some(v) = map_num(m, k) {
                    days += v * scale;
                }
            }
            let whole_days = days.trunc();
            nanos += days.fract() * NANOS_PER_DAY;
            for (k, scale) in NANO_FIELDS {
                if let Some(v) = map_num(m, k) {
                    nanos += v * scale;
                }
            }
            Ok(Temporal::Duration {
                months: f64_to_i64_range(whole_months)?,
                days: f64_to_i64_range(whole_days)?,
                nanos: f64_to_i128_range(nanos.round())?,
            })
        }
        _ => Err(Error::invalid_argument_type("duration()")),
    }
}

fn inherit_offset(value: &Temporal) -> ResolvedZone {
    match value {
        Temporal::DateTime { offset, zone, .. } => ResolvedZone {
            offset: *offset,
            zone: zone.clone(),
        },
        Temporal::Time { offset, .. } => ResolvedZone {
            offset: *offset,
            zone: None,
        },
        _ => ResolvedZone::utc(),
    }
}

fn apply_date_extras(
    d: NaiveDate,
    m: &std::collections::BTreeMap<String, Value>,
) -> Result<NaiveDate> {
    let mut out = d;
    if let Some(day) = map_i(m, "day") {
        let day = u32::try_from(day).map_err(|_| Error::ty("invalid date"))?;
        out = date_from_parts(out.year(), out.month(), day)?;
    }
    if let Some(dow) = map_i(m, "dayOfWeek").or_else(|| map_i(m, "dayofweek")) {
        let current = i64::from(out.weekday().number_from_monday());
        let delta = dow.checked_sub(current).ok_or_else(range_err)?;
        let dur = ChronoDur::try_days(delta).ok_or_else(range_err)?;
        out = out.checked_add_signed(dur).ok_or_else(range_err)?;
    }
    Ok(out)
}

fn apply_time_extras(
    t: NaiveTime,
    m: &std::collections::BTreeMap<String, Value>,
) -> Result<NaiveTime> {
    let mut hour = t.hour();
    let mut minute = t.minute();
    let mut second = t.second();
    let nano = t.nanosecond();
    if let Some(h) = map_i(m, "hour") {
        hour = u32::try_from(h).map_err(|_| Error::ty("invalid time"))?;
    }
    if let Some(mi) = map_i(m, "minute") {
        minute = u32::try_from(mi).map_err(|_| Error::ty("invalid time"))?;
    }
    if let Some(s) = map_i(m, "second") {
        second = u32::try_from(s).map_err(|_| Error::ty("invalid time"))?;
    }
    let nano = nano_with_subsecond_fields(nano, m)?;
    NaiveTime::from_hms_nano_opt(hour, minute, second, nano)
        .ok_or_else(|| Error::ty("invalid time"))
}

/// Truncation unit vocabulary, parsed once at the `truncate` entry so the
/// date/time helpers match on a typed unit instead of re-classifying names.
/// The truncate cluster (this enum through `truncate_time`) is the next
/// extraction into `temporal/truncate.rs` when this file approaches the
/// file-size budget (ADR-0340).
#[derive(Clone, Copy)]
enum TruncUnit {
    Millennium,
    Century,
    Decade,
    Year,
    WeekYear,
    Quarter,
    Month,
    Week,
    Day,
    Hour,
    Minute,
    Second,
    Millisecond,
    Microsecond,
    Nanosecond,
}

impl TruncUnit {
    fn parse(unit: &str) -> Result<TruncUnit> {
        Ok(match unit {
            "millennium" => TruncUnit::Millennium,
            "century" => TruncUnit::Century,
            "decade" => TruncUnit::Decade,
            "year" => TruncUnit::Year,
            "weekyear" => TruncUnit::WeekYear,
            "quarter" => TruncUnit::Quarter,
            "month" => TruncUnit::Month,
            "week" => TruncUnit::Week,
            "day" => TruncUnit::Day,
            "hour" => TruncUnit::Hour,
            "minute" => TruncUnit::Minute,
            "second" => TruncUnit::Second,
            "millisecond" => TruncUnit::Millisecond,
            "microsecond" => TruncUnit::Microsecond,
            "nanosecond" => TruncUnit::Nanosecond,
            _ => return Err(Error::ty("unknown truncate unit")),
        })
    }

    /// The nanosecond quantum `truncate_time` rounds down to: the unit's own
    /// length, or a whole day for date-granularity units (which zero the
    /// time part).
    fn time_quantum(self) -> i64 {
        match self {
            TruncUnit::Hour => 3_600_000_000_000,
            TruncUnit::Minute => 60_000_000_000,
            TruncUnit::Second => 1_000_000_000,
            TruncUnit::Millisecond => 1_000_000,
            TruncUnit::Microsecond => 1_000,
            TruncUnit::Nanosecond => 1,
            _ => 86_400_000_000_000,
        }
    }
}

pub fn truncate(kind: &str, unit: &str, value: &Temporal, map: Option<&Value>) -> Result<Temporal> {
    let extra = match map {
        Some(Value::Map(m)) => Some(m),
        Some(Value::Null) | None => None,
        _ => return Err(Error::ty("truncate map")),
    };
    // `Value::Map` shares its map behind an `Arc`; the helpers take the map.
    let extra = extra.map(|m| &**m);
    let unit = TruncUnit::parse(&unit.to_ascii_lowercase())?;
    match kind {
        "date" => {
            let d = date_of(value)?;
            let mut out = truncate_date(d, unit)?;
            if let Some(m) = extra {
                out = apply_date_extras(out, m)?;
            }
            Ok(Temporal::Date(out))
        }
        "localdatetime" => Ok(Temporal::LocalDateTime(truncate_to_local(
            value, unit, extra,
        )?)),
        "datetime" => {
            let out = truncate_to_local(value, unit, extra)?;
            let resolved = resolve_offset(extra, value, IanaAt::Local(out))?;
            let utc = sub_offset(out, resolved.offset)?;
            Ok(Temporal::DateTime {
                utc,
                offset: resolved.offset,
                zone: resolved.zone,
            })
        }
        "localtime" | "time" => {
            let tm = time_of(value).ok_or_else(|| Error::ty("truncate time"))?;
            let mut tm = truncate_time(tm, unit);
            if let Some(m) = extra {
                tm = apply_time_extras(tm, m)?;
            }
            if kind == "time" {
                // A `time` truncate carries no instant of its own, so a
                // `timezone` in the extras map is resolved at the statement's
                // pinned UTC instant (`now_utc_dt`, the `now_*` family's
                // member) — and that instant is read as a UTC instant
                // (`IanaAt::Utc`), not as a UTC wall clock re-read in the
                // zone.
                let resolved = resolve_offset(extra, value, IanaAt::Utc(now_utc_dt()))?;
                Ok(Temporal::Time {
                    time: tm,
                    offset: resolved.offset,
                })
            } else {
                Ok(Temporal::LocalTime(tm))
            }
        }
        _ => Err(Error::ty("truncate kind")),
    }
}

/// Truncate `value`'s local wall clock to `unit`, then apply the extras
/// map's date and time fields to the truncated result.
fn truncate_to_local(
    value: &Temporal,
    unit: TruncUnit,
    extra: Option<&std::collections::BTreeMap<String, Value>>,
) -> Result<NaiveDateTime> {
    let dt = local_dt_of(value)?;
    let mut out = truncate_date(dt.date(), unit)?.and_time(truncate_time(dt.time(), unit));
    if let Some(m) = extra {
        out = apply_date_extras(out.date(), m)?.and_time(apply_time_extras(out.time(), m)?);
    }
    Ok(out)
}

/// The zone a truncation result keeps: an explicit `timezone` in the extras
/// map wins (resolved at `at`), otherwise the input value's own.
fn resolve_offset(
    extra: Option<&std::collections::BTreeMap<String, Value>>,
    value: &Temporal,
    at: IanaAt,
) -> Result<ResolvedZone> {
    match extra.filter(|m| m.contains_key("timezone")) {
        Some(m) => offset_from_map(m, at),
        None => Ok(inherit_offset(value)),
    }
}

fn truncate_date(d: NaiveDate, unit: TruncUnit) -> Result<NaiveDate> {
    Ok(match unit {
        TruncUnit::Millennium => date_from_parts(d.year() / 1000 * 1000, 1, 1)?,
        TruncUnit::Century => date_from_parts(d.year() / 100 * 100, 1, 1)?,
        TruncUnit::Decade => date_from_parts(d.year() / 10 * 10, 1, 1)?,
        TruncUnit::Year => date_from_parts(d.year(), 1, 1)?,
        TruncUnit::WeekYear => {
            let iso = d.iso_week();
            date_from_week(iso.year(), 1, Weekday::Mon)?
        }
        TruncUnit::Quarter => {
            let q = (d.month() - 1) / 3 * 3 + 1;
            date_from_parts(d.year(), q, 1)?
        }
        TruncUnit::Month => date_from_parts(d.year(), d.month(), 1)?,
        TruncUnit::Week => {
            // `NaiveDate - TimeDelta` panics when the Monday of this week
            // falls before `NaiveDate::MIN`; use the checked operator.
            let back = i64::from(d.weekday().num_days_from_monday());
            d.checked_sub_signed(ChronoDur::days(back))
                .ok_or_else(range_err)?
        }
        // Day and every time-granularity unit leave the date untouched.
        _ => d,
    })
}

fn truncate_time(t: NaiveTime, unit: TruncUnit) -> NaiveTime {
    let q = unit.time_quantum();
    let frac = t.nanosecond();
    // chrono encodes a leap second as second 59 with a fraction >= 1e9, i.e.
    // past the end of the day on the nanosecond timeline, where the quantum
    // formula below would wrap it to midnight. Truncate the represented
    // second instead, dropping the leap marker for second-or-coarser units.
    if frac >= 1_000_000_000 {
        let secs = t.num_seconds_from_midnight();
        let (secs, frac) = if q >= 1_000_000_000 {
            let q_secs = (q / 1_000_000_000) as u32;
            (secs - secs % q_secs, 0)
        } else {
            let sub = frac - 1_000_000_000;
            (secs, 1_000_000_000 + (sub - sub.rem_euclid(q as u32)))
        };
        return NaiveTime::from_num_seconds_from_midnight_opt(secs, frac).unwrap_or(NaiveTime::MIN);
    }
    let n = time_to_nanos(t);
    nanos_to_time(n - n.rem_euclid(q))
}
