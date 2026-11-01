//! OpenCypher temporal values (ADR-0021).

use std::cmp::Ordering;
use std::fmt;

use chrono::{
    Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, Offset, Timelike, Utc, Weekday,
};

use crate::{Error, Value};

mod arith;
mod construct;
mod format;
mod parse;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_ranges;
mod zone;

use zone::{add_offset, zone_kind, ZoneSpec};

pub use arith::{
    add_duration, civil_epoch_days, civil_from_days, duration_between, duration_between_unit,
    scale_duration, sub_duration, BetweenUnit,
};
pub use construct::{
    construct_date, construct_datetime, construct_duration, construct_localdatetime,
    construct_localtime, construct_time, truncate,
};
pub use format::format_pattern;
pub use parse::{datetime_from_epoch, datetime_from_epoch_millis, parse_iso, parse_pattern};
pub use zone::{zone_offset_at, zone_offset_at_utc, zone_region, ZoneError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Temporal {
    Date(NaiveDate),
    /// Out-of-range proleptic date (TCK ±999999999).
    CivilDate {
        year: i64,
        month: u32,
        day: u32,
    },
    LocalTime(NaiveTime),
    Time {
        time: NaiveTime,
        offset: i32,
    },
    LocalDateTime(NaiveDateTime),
    CivilDateTime {
        year: i64,
        month: u32,
        day: u32,
        time: NaiveTime,
    },
    DateTime {
        utc: NaiveDateTime,
        offset: i32,
        zone: Option<String>,
    },
    Duration {
        months: i64,
        days: i64,
        nanos: i128,
    },
}

/// The zone half of a `DateTime`'s equality identity (TCK TemporalCompare):
/// an IANA zone is identified by its name — so the two halves of a DST
/// fall-back overlap stay equal — and every other spelling (`Z`, `+00:00`,
/// `UTC`, …) by the UTC offset it denotes, so `Z` equals `+00:00` while a
/// named zone never equals the offset it happens to denote.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ZoneId {
    Named(String),
    Offset(i32),
}

impl ZoneId {
    /// The identity of a `DateTime`'s `zone` at `offset`.
    pub fn of(zone: Option<&str>, offset: i32) -> ZoneId {
        match zone_region(zone) {
            Some(name) => ZoneId::Named(name.to_string()),
            None => ZoneId::Offset(offset),
        }
    }
}

impl Temporal {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Temporal::Date(_) | Temporal::CivilDate { .. } => "date",
            Temporal::LocalTime(_) => "localtime",
            Temporal::Time { .. } => "time",
            Temporal::LocalDateTime(_) | Temporal::CivilDateTime { .. } => "localdatetime",
            Temporal::DateTime { .. } => "datetime",
            Temporal::Duration { .. } => "duration",
        }
    }

    pub fn iso_string(&self) -> String {
        self.render(false)
    }

    /// The full-precision ISO rendering: [`Temporal::iso_string`] with
    /// the time-of-day's seconds always present. The value layer's
    /// temporal rendering policy for every string-producing surface —
    /// `toString` (top-level, nested in a map or accepted list, or
    /// through `toStringOrNull`) and the interpolated-string scalar
    /// path — one rendering for one value, however it was built or
    /// wherever it is embedded (ADR-0448).
    pub fn full_seconds_string(&self) -> String {
        self.render(true)
    }

    /// The one ISO renderer both public forms delegate to: identical
    /// except for the time-of-day's seconds-omission rule
    /// (`format_time`'s `%H:%M` truncation for a whole minute), which
    /// `full_seconds` disables.
    fn render(&self, full_seconds: bool) -> String {
        match self {
            Temporal::Date(d) => d.format("%Y-%m-%d").to_string(),
            Temporal::CivilDate { year, month, day } => format_civil_date(*year, *month, *day),
            Temporal::LocalTime(t) => format_time(*t, full_seconds),
            Temporal::Time { time, offset } => {
                format!(
                    "{}{}",
                    format_time(*time, full_seconds),
                    format_zone_suffix(*offset, None)
                )
            }
            Temporal::LocalDateTime(dt) => format_local_dt(*dt, full_seconds),
            Temporal::CivilDateTime {
                year,
                month,
                day,
                time,
            } => format!(
                "{}T{}",
                format_civil_date(*year, *month, *day),
                format_time(*time, full_seconds)
            ),
            Temporal::DateTime { utc, offset, zone } => {
                // A deliberate, stated fallback — not a silent one: the
                // Bolt parameter decoders accept any i32 offset, so a
                // crafted or extreme `utc + offset` can leave chrono's
                // range (constructors cannot, parameters can); rendering
                // has no error channel and must not panic on a decodable
                // value, so the UTC wall clock renders beside the
                // offset/zone suffix. (`local_of` is the identity
                // helper's stricter form; render does not use it.)
                let local = add_offset(*utc, *offset).unwrap_or(*utc);
                format!(
                    "{}{}",
                    format_local_dt(local, full_seconds),
                    format_zone_suffix(*offset, zone.as_deref())
                )
            }
            Temporal::Duration {
                months,
                days,
                nanos,
            } => format_duration(*months, *days, *nanos),
        }
    }

    pub fn component(&self, key: &str) -> Result<Option<Value>, Error> {
        let k = key.to_ascii_lowercase();
        match self {
            Temporal::Duration {
                months,
                days,
                nanos,
            } => duration_component(*months, *days, *nanos, &k),
            other => Ok(calendar_component(other, &k)),
        }
    }

    /// Cypher `=` between two values of the same temporal kind (`false` for
    /// unlike kinds). The relation is deliberately *not* the ordering one:
    ///
    /// * `Time` — local time **and** offset, so `12:00+01:00` and `11:00Z`
    ///   (the same instant) are different times, while `12:00Z` and
    ///   `12:00+00:00` are the same.
    /// * `DateTime` — local date-time **and** zone identity, so the two
    ///   halves of a DST fall-back overlap (`02:30+02:00[Europe/Stockholm]`
    ///   and `02:30+01:00[Europe/Stockholm]`) are equal even though they are
    ///   different instants, while `[Europe/Stockholm]` never equals
    ///   `+01:00` (`ZoneId`).
    pub fn equals(&self, other: &Temporal) -> bool {
        match (self, other) {
            (Temporal::Date(a), Temporal::Date(b)) => a == b,
            (
                Temporal::CivilDate {
                    year: ya,
                    month: ma,
                    day: da,
                },
                Temporal::CivilDate {
                    year: yb,
                    month: mb,
                    day: db,
                },
            ) => (ya, ma, da) == (yb, mb, db),
            (Temporal::LocalTime(a), Temporal::LocalTime(b)) => a == b,
            (
                Temporal::Time {
                    time: a,
                    offset: oa,
                },
                Temporal::Time {
                    time: b,
                    offset: ob,
                },
            ) => a == b && oa == ob,
            (Temporal::LocalDateTime(a), Temporal::LocalDateTime(b)) => a == b,
            (
                Temporal::CivilDateTime {
                    year: ya,
                    month: ma,
                    day: da,
                    time: ta,
                },
                Temporal::CivilDateTime {
                    year: yb,
                    month: mb,
                    day: db,
                    time: tb,
                },
            ) => (ya, ma, da, ta) == (yb, mb, db, tb),
            (Temporal::DateTime { .. }, Temporal::DateTime { .. }) => {
                self.datetime_eq_identity() == other.datetime_eq_identity()
            }
            (
                Temporal::Duration {
                    months: am,
                    days: ad,
                    nanos: an,
                },
                Temporal::Duration {
                    months: bm,
                    days: bd,
                    nanos: bn,
                },
            ) => (am, ad, an) == (bm, bd, bn),
            _ => false,
        }
    }

    /// The two components [`Self::equals`] compares for a zoned `DateTime`:
    /// its local wall clock and its [`ZoneId`]. `None` for every other
    /// temporal kind. Exposed so a caller that buckets values by equality —
    /// the executor's property index, the uniqueness probe, `MERGE` — derives
    /// *this* identity instead of re-deriving it: `a = b` must imply one
    /// bucket, which a key built from the UTC instant alone does not give
    /// (the two halves of a DST fall-back overlap share a local wall clock
    /// and zone but not an instant).
    pub fn datetime_eq_identity(&self) -> Option<(NaiveDateTime, ZoneId)> {
        match self {
            Temporal::DateTime { utc, offset, zone } => Some((
                local_of(*utc, *offset),
                ZoneId::of(zone.as_deref(), *offset),
            )),
            _ => None,
        }
    }

    /// Three-valued ordering for `<`, `>`, `<=`, `>=` (`None` is the null
    /// result of comparing incomparable values):
    ///
    /// * `Time` — UTC nanos-of-day (not wrapped at midnight, so it can be
    ///   negative), then the offset, so two times at the same instant still
    ///   order.
    /// * `DateTime` — instant (nanos included), then offset, then a plain
    ///   offset before a named zone, then the zone name.
    /// * `Duration` — not orderable at all; only `=` is defined for it.
    pub fn compare(&self, other: &Temporal) -> Option<Ordering> {
        match (self, other) {
            (Temporal::Date(a), Temporal::Date(b)) => Some(a.cmp(b)),
            (
                Temporal::CivilDate {
                    year: ya,
                    month: ma,
                    day: da,
                },
                Temporal::CivilDate {
                    year: yb,
                    month: mb,
                    day: db,
                },
            ) => Some((ya, ma, da).cmp(&(yb, mb, db))),
            (Temporal::LocalTime(a), Temporal::LocalTime(b)) => Some(a.cmp(b)),
            (Temporal::LocalDateTime(a), Temporal::LocalDateTime(b)) => Some(a.cmp(b)),
            (
                Temporal::Time {
                    time: a,
                    offset: oa,
                },
                Temporal::Time {
                    time: b,
                    offset: ob,
                },
            ) => Some(
                utc_time_nanos(*a, *oa)
                    .cmp(&utc_time_nanos(*b, *ob))
                    .then(oa.cmp(ob)),
            ),
            (
                Temporal::DateTime {
                    utc: ua,
                    offset: oa,
                    zone: za,
                },
                Temporal::DateTime {
                    utc: ub,
                    offset: ob,
                    zone: zb,
                },
            ) => Some(
                ua.cmp(ub)
                    .then_with(|| oa.cmp(ob))
                    // A named (IANA) zone sorts after the plain offset it
                    // equals, by name: `None < Some(_)`.
                    .then_with(|| zone_region(za.as_deref()).cmp(&zone_region(zb.as_deref()))),
            ),
            (Temporal::Duration { .. }, Temporal::Duration { .. }) => None,
            _ => None,
        }
    }

    /// Total order for `ORDER BY`, `min` and `max`: [`Self::compare`] where it
    /// is defined, the `(months, days, nanos)` triple for durations (which
    /// equality alone cannot rank), and `Equal` across unlike kinds — the
    /// callers keep unlike temporals distinct by the value type rank.
    pub(crate) fn total_cmp(&self, other: &Temporal) -> Ordering {
        match (self, other) {
            (
                Temporal::Duration {
                    months: am,
                    days: ad,
                    nanos: an,
                },
                Temporal::Duration {
                    months: bm,
                    days: bd,
                    nanos: bn,
                },
            ) => (am, ad, an).cmp(&(bm, bd, bn)),
            _ => self.compare(other).unwrap_or(Ordering::Equal),
        }
    }
}

impl fmt::Display for Temporal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.iso_string())
    }
}

/// The signed, zero-padded civil-date form both civil variants render:
/// a negative year is zero-padded to nine digits behind the sign.
fn format_civil_date(year: i64, month: u32, day: u32) -> String {
    if year < 0 {
        format!("-{year:09}-{month:02}-{day:02}", year = year.unsigned_abs())
    } else {
        format!("+{year}-{month:02}-{day:02}")
    }
}

/// The time-of-day renderer: `full_seconds` disables the whole-minute
/// truncation (`%H:%M`) — the one place the seconds-omission fact
/// lives, consulted by both of `render`'s modes (ADR-0448).
fn format_time(t: NaiveTime, full_seconds: bool) -> String {
    let n = t.nanosecond();
    if n == 0 {
        if t.second() == 0 && !full_seconds {
            t.format("%H:%M").to_string()
        } else {
            t.format("%H:%M:%S").to_string()
        }
    } else {
        let s = format!("{n:09}");
        let trim = s.trim_end_matches('0');
        format!(
            "{:02}:{:02}:{:02}.{}",
            t.hour(),
            t.minute(),
            t.second(),
            trim
        )
    }
}

fn format_local_dt(dt: NaiveDateTime, full_seconds: bool) -> String {
    format!(
        "{}T{}",
        dt.date().format("%Y-%m-%d"),
        format_time(dt.time(), full_seconds)
    )
}

fn format_offset(secs: i32) -> String {
    let sign = if secs >= 0 { '+' } else { '-' };
    let abs = secs.unsigned_abs();
    let h = abs / 3600;
    let m = (abs % 3600) / 60;
    let s = abs % 60;
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else if m == 0 {
        format!("{sign}{h:02}:00")
    } else {
        format!("{sign}{h:02}:{m:02}")
    }
}

fn format_zone_suffix(offset: i32, zone: Option<&str>) -> String {
    // The IANA arm is the canonical classifier's answer; the remaining
    // spellings (UTC, a numeric offset, anything else) keep their rule.
    if let Some(name) = zone_region(zone) {
        return format!("{}[{name}]", format_offset(offset));
    }
    match zone.map(zone_kind) {
        // `zone_region` already answered the IANA arm.
        Some(ZoneSpec::Utc) => "Z".into(),
        Some(ZoneSpec::Fixed(z)) => z.to_string(),
        // Non-canonical spellings keep the stored numeric offset.
        Some(ZoneSpec::Iana(_) | ZoneSpec::Other(_)) | None => {
            if offset == 0 {
                "Z".into()
            } else {
                format_offset(offset)
            }
        }
    }
}

fn format_duration(months: i64, days: i64, nanos: i128) -> String {
    if months == 0 && days == 0 && nanos == 0 {
        return "PT0S".into();
    }
    let mut s = String::from("P");
    let y = months / 12;
    let mo = months % 12;
    if y != 0 {
        s.push_str(&format!("{y}Y"));
    }
    if mo != 0 {
        s.push_str(&format!("{mo}M"));
    }
    if days != 0 {
        s.push_str(&format!("{days}D"));
    }
    let neg_t = nanos < 0;
    let mut n = nanos.unsigned_abs();
    let hours = n / 3_600_000_000_000;
    n %= 3_600_000_000_000;
    let mins = n / 60_000_000_000;
    n %= 60_000_000_000;
    let secs = n / 1_000_000_000;
    let nano = n % 1_000_000_000;
    if hours != 0 || mins != 0 || secs != 0 || nano != 0 {
        s.push('T');
        let sign = if neg_t { "-" } else { "" };
        if hours != 0 {
            s.push_str(&format!("{sign}{hours}H"));
        }
        if mins != 0 {
            s.push_str(&format!("{sign}{mins}M"));
        }
        if secs != 0 || nano != 0 {
            if nano == 0 {
                s.push_str(&format!("{sign}{secs}S"));
            } else {
                let frac = format!("{nano:09}").trim_end_matches('0').to_string();
                s.push_str(&format!("{sign}{secs}.{frac}S"));
            }
        }
    }
    s
}

fn time_to_nanos(t: NaiveTime) -> i64 {
    t.num_seconds_from_midnight() as i64 * 1_000_000_000 + t.nanosecond() as i64
}

/// Nanoseconds of the UTC clock time a `Time` denotes. Deliberately *not*
/// wrapped at midnight: an offset can push the wall clock out of the day, and
/// the ordering contract is this signed value plus the offset.
fn utc_time_nanos(t: NaiveTime, offset: i32) -> i64 {
    time_to_nanos(t) - i64::from(offset) * 1_000_000_000
}

/// The wall clock a zoned instant reads at (`utc + offset`). Constructors
/// keep the sum inside chrono's range, but the Bolt parameter decoders
/// accept any i32 offset (the render path's comment at the `DateTime` arm
/// documents this input class), and equality bucketing has no error
/// channel — so an out-of-range sum degrades to the UTC wall clock, the
/// same stated fallback render uses, instead of panicking. Deterministic
/// per payload, so `a = b` still implies one bucket.
fn local_of(utc: NaiveDateTime, offset: i32) -> NaiveDateTime {
    add_offset(utc, offset).unwrap_or(utc)
}

/// `n` is always a within-day nanosecond count (rem_euclid at the call sites),
/// so the conversion is infallible in practice; fall back instead of unwrap.
fn nanos_to_time(n: i64) -> NaiveTime {
    let secs = (n / 1_000_000_000) as u32;
    let nano = (n % 1_000_000_000) as u32;
    NaiveTime::from_num_seconds_from_midnight_opt(secs, nano).unwrap_or(NaiveTime::MIN)
}

/// Error returned when a temporal computation overflows the representable
/// range (chrono dates ±262143 years, i64 duration months/days, i128 nanos).
/// The honest contract for out-of-range results is an error, never a panic.
pub(super) fn range_err() -> Error {
    Error::ty("temporal value out of range")
}

/// The `Temporal` payload a field map holds under `k`, or `None` when the
/// key is absent or non-temporal. Shared by the constructors and the zone
/// resolution (`construct.rs`, `zone.rs`); the scalar extraction is the
/// canonical [`Value::as_temporal`].
pub(super) fn map_temporal<'a>(
    map: &'a std::collections::BTreeMap<String, Value>,
    k: &str,
) -> Option<&'a Temporal> {
    map.get(k).and_then(Value::as_temporal)
}

/// f64 → i64 with an honest range error instead of a saturating cast, so
/// accumulated duration fields can never wrap or saturate silently into a
/// later overflow. (`i64::MAX as f64` rounds up to 2^63, so the strict
/// inequalities keep the `as` cast exact at the boundaries.)
pub(super) fn f64_to_i64_range(v: f64) -> Result<i64, Error> {
    if v.is_finite() && v > i64::MIN as f64 && v < i64::MAX as f64 {
        Ok(v as i64)
    } else {
        Err(range_err())
    }
}

/// f64 → i128 with a range error; 1e38 is comfortably below i128::MAX ≈ 1.7e38.
pub(super) fn f64_to_i128_range(v: f64) -> Result<i128, Error> {
    if v.is_finite() && v.abs() < 1.0e38 {
        Ok(v as i128)
    } else {
        Err(range_err())
    }
}

fn duration_component(
    months: i64,
    days: i64,
    nanos: i128,
    k: &str,
) -> Result<Option<Value>, Error> {
    let hours = nanos / 3_600_000_000_000;
    let rem = nanos % 3_600_000_000_000;
    let minutes = rem / 60_000_000_000;
    let rem = rem % 60_000_000_000;
    let seconds = rem / 1_000_000_000;
    let _nano = rem % 1_000_000_000;
    // Totals derived from the i128 nanosecond count can exceed i64 for
    // extreme durations; error honestly instead of truncating through `as`.
    let total = |v: i128| i64::try_from(v).map_err(|_| range_err());
    Ok(Some(Value::Int(match k {
        "years" => months / 12,
        "months" => months,
        "monthsofyear" => months.rem_euclid(12),
        "quarters" => months / 3,
        "quartersofyear" => months.rem_euclid(12) / 3,
        "monthsofquarter" => months.rem_euclid(3),
        "weeksofmonth" | "weeks" => days / 7,
        "days" => days,
        "daysofweek" => days.rem_euclid(7),
        "hours" => total(hours)?,
        "minutes" => total(minutes + hours * 60)?,
        "minutesofhour" => total(minutes)?,
        "seconds" => total(nanos.div_euclid(1_000_000_000))?,
        "secondsofminute" => total(seconds)?,
        "nanoseconds" => total(nanos)?,
        "nanosecondsofsecond" => total(nanos.rem_euclid(1_000_000_000))?,
        "milliseconds" => total(nanos / 1_000_000)?,
        "microseconds" => total(nanos / 1_000)?,
        "millisecondsofsecond" => total(nanos.rem_euclid(1_000_000_000) / 1_000_000)?,
        "microsecondsofsecond" => total(nanos.rem_euclid(1_000_000_000) / 1_000)?,
        _ => return Ok(None),
    })))
}

fn calendar_component(t: &Temporal, k: &str) -> Option<Value> {
    let (date, time, offset) = match t {
        Temporal::Date(d) => (Some(*d), None, None),
        Temporal::LocalTime(tm) => (None, Some(*tm), None),
        Temporal::Time { time, offset } => (None, Some(*time), Some(*offset)),
        Temporal::LocalDateTime(dt) => (Some(dt.date()), Some(dt.time()), None),
        Temporal::DateTime { utc, offset, .. } => {
            let local = add_offset(*utc, *offset).ok()?;
            (Some(local.date()), Some(local.time()), Some(*offset))
        }
        Temporal::Duration { .. } | Temporal::CivilDate { .. } | Temporal::CivilDateTime { .. } => {
            return None
        }
    };
    if k == "timezone" {
        if let Temporal::DateTime {
            zone: Some(z),
            offset: off,
            ..
        } = t
        {
            return Some(Value::String(match zone_kind(z) {
                ZoneSpec::Iana(name) | ZoneSpec::Fixed(name) => name.into(),
                ZoneSpec::Utc => "Z".into(),
                // Non-canonical spellings keep the stored numeric offset.
                ZoneSpec::Other(_) => format_offset(*off).into(),
            }));
        }
        return offset.map(|o| Value::String(format_offset(o).into()));
    }
    if k == "epochseconds" || k == "epochmillis" {
        // One handler for both epoch keys: a DateTime stamps its stored UTC
        // instant; any other dated value stamps its date at midnight.
        let stamped = match t {
            Temporal::DateTime { utc, .. } => Some(*utc),
            _ => date.and_then(|d| d.and_hms_opt(0, 0, 0)),
        };
        return stamped.map(|dt| {
            let dt = dt.and_utc();
            let secs = dt.timestamp();
            Value::Int(if k == "epochseconds" {
                secs
            } else {
                secs * 1000 + i64::from(dt.timestamp_subsec_millis())
            })
        });
    }
    if k == "offset" || k == "offsetseconds" || k == "offsetminutes" {
        return match (k, offset) {
            ("offset", Some(o)) => Some(Value::String(format_offset(o).into())),
            ("offsetseconds", Some(o)) => Some(Value::Int(o as i64)),
            ("offsetminutes", Some(o)) => Some(Value::Int((o / 60) as i64)),
            _ => None,
        };
    }
    if let Some(d) = date {
        let v = match k {
            "year" => d.year() as i64,
            "month" => d.month() as i64,
            "day" => d.day() as i64,
            "ordinalday" => d.ordinal() as i64,
            "quarter" => ((d.month() - 1) / 3 + 1) as i64,
            "week" => d.iso_week().week() as i64,
            "weekyear" => d.iso_week().year() as i64,
            "dayofweek" | "weekday" => d.weekday().number_from_monday() as i64,
            "dayofquarter" => {
                let qstart = (d.month() - 1) / 3 * 3 + 1;
                let start = NaiveDate::from_ymd_opt(d.year(), qstart, 1)?;
                (d - start).num_days() + 1
            }
            _ => {
                if let Some(tm) = time {
                    return time_component(tm, k);
                }
                return None;
            }
        };
        return Some(Value::Int(v));
    }
    time.and_then(|tm| time_component(tm, k))
}

fn time_component(t: NaiveTime, k: &str) -> Option<Value> {
    Some(Value::Int(match k {
        "hour" => t.hour() as i64,
        "minute" => t.minute() as i64,
        "second" => t.second() as i64,
        "millisecond" => (t.nanosecond() / 1_000_000) as i64,
        "microsecond" => (t.nanosecond() / 1_000) as i64,
        "nanosecond" => t.nanosecond() as i64,
        _ => return None,
    }))
}

thread_local! {
    static PINNED: std::cell::Cell<Option<(NaiveDateTime, NaiveDateTime, i32)>> =
        const { std::cell::Cell::new(None) };
}

/// Pins `date()` / `datetime()` / `time()` to one instant for a statement.
pub struct QueryClockGuard;

impl Default for QueryClockGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryClockGuard {
    pub fn new() -> Self {
        let utc = Utc::now();
        let local = Local::now();
        PINNED.with(|c| {
            c.set(Some((
                utc.naive_utc(),
                local.naive_local(),
                local.offset().fix().local_minus_utc(),
            )));
        });
        Self
    }
}

/// Test-only: pins the statement clock to an explicit instant instead of
/// the wall clock, so a test can pick a transition-adjacent instant. The
/// instant is read at offset 0 — the same reading `now_dt()` gives.
#[cfg(test)]
impl QueryClockGuard {
    pub fn pinned_at(utc: NaiveDateTime) -> Self {
        PINNED.with(|c| c.set(Some((utc, utc, 0))));
        Self
    }
}

impl Drop for QueryClockGuard {
    fn drop(&mut self) {
        PINNED.with(|c| c.set(None));
    }
}

fn pinned() -> (NaiveDateTime, NaiveDateTime, i32) {
    PINNED.with(|c| {
        c.get().unwrap_or_else(|| {
            let utc = Utc::now();
            let local = Local::now();
            (
                utc.naive_utc(),
                local.naive_local(),
                local.offset().fix().local_minus_utc(),
            )
        })
    })
}

pub fn now_date() -> Temporal {
    Temporal::Date(pinned().1.date())
}

pub fn now_local_dt() -> Temporal {
    Temporal::LocalDateTime(pinned().1)
}

pub fn now_dt() -> Temporal {
    Temporal::DateTime {
        utc: pinned().0,
        offset: 0,
        zone: Some("Z".into()),
    }
}

/// The pinned statement instant as milliseconds since 1970-01-01T00:00:00Z,
/// the same instant `now_dt()` reads (so `timestamp()` equals
/// `datetime().epochMillis` for the whole statement).
pub fn now_epoch_millis() -> i64 {
    pinned().0.and_utc().timestamp_millis()
}

pub fn now_local_time() -> Temporal {
    Temporal::LocalTime(pinned().1.time())
}

pub fn now_time() -> Temporal {
    let (_, local, offset) = pinned();
    Temporal::Time {
        time: local.time(),
        offset,
    }
}

/// The pinned statement instant's UTC wall clock — the `now_*` family's
/// naive-UTC member, for callers that resolve a zone at "now" without
/// constructing a `Temporal` (the `time` truncate boundary).
pub fn now_utc_dt() -> NaiveDateTime {
    pinned().0
}

fn weekday_from_i(i: i64) -> Weekday {
    // `i.rem_euclid(7)` is total (unlike `(i - 1).rem_euclid(7)`, which
    // overflows at i64::MIN) and selects the same weekday for every input:
    // 1..=6 -> Mon..Sat, 0 -> Sun.
    match i.rem_euclid(7) {
        1 => Weekday::Mon,
        2 => Weekday::Tue,
        3 => Weekday::Wed,
        4 => Weekday::Thu,
        5 => Weekday::Fri,
        6 => Weekday::Sat,
        _ => Weekday::Sun,
    }
}

const AVG_DAYS_PER_MONTH: f64 = 365.2425 / 12.0;
const NANOS_PER_DAY: f64 = 86_400_000_000_000.0;
