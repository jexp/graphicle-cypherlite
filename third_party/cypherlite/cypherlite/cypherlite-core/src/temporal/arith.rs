//! Temporal arithmetic: duration between, add/subtract, and scaling.
//!
//! Every computation that touches a user-controlled magnitude (duration
//! components, civil-date years, offsets) goes through checked arithmetic or
//! chrono's `try_*` / `checked_*` constructors and returns
//! `Err(Error::ty(..))` on out-of-range results. Nothing in this module may
//! panic on query input.

use chrono::{Datelike, Duration as ChronoDur, NaiveDate, NaiveDateTime, NaiveTime};

use crate::{Error, Result};

use super::zone::{add_offset, sub_offset, zone_offset_at, zone_region};
use super::{nanos_to_time, range_err, time_to_nanos, Temporal, AVG_DAYS_PER_MONTH, NANOS_PER_DAY};

/// Days from 1970-01-01 to `year-month-day` in the proleptic Gregorian
/// calendar (days-from-civil over i128, so civil years — i64, parsed
/// unbounded up to ±i64::MAX — cannot overflow). The one civil-day count:
/// civil-date arithmetic here and the Bolt Date struct (ADR-0325) use it.
pub fn civil_epoch_days(year: i64, month: u32, day: u32) -> i128 {
    let mut y = i128::from(year);
    let mut m = i128::from(month);
    if m <= 2 {
        y -= 1;
        m += 9;
    } else {
        m -= 3;
    }
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + i128::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `dt + d`, erroring instead of panicking when the result is out of chrono
/// range or the duration itself is out of `TimeDelta` bounds.
fn checked_add_dur(dt: NaiveDateTime, d: Option<ChronoDur>) -> Result<NaiveDateTime> {
    let d = d.ok_or_else(range_err)?;
    dt.checked_add_signed(d).ok_or_else(range_err)
}

fn iana_zone(t: &Temporal) -> Option<&str> {
    match t {
        Temporal::DateTime { zone, .. } => zone_region(zone.as_deref()),
        _ => None,
    }
}

fn utc_in_zone(local: NaiveDateTime, zone: &str) -> Option<NaiveDateTime> {
    // The single-or-earliest rule is the exported resolver (ADR-0178); its
    // gap/unknown-zone outcomes collapse to this path's `None`, and the
    // checked subtraction keeps an out-of-range `local - offset` a `None`.
    let offset = zone_offset_at(zone, local).ok()?;
    local.checked_sub_signed(ChronoDur::seconds(i64::from(offset)))
}

fn shift_by_offset(dt: NaiveDateTime, offset: i32) -> Option<NaiveDateTime> {
    dt.checked_add_signed(ChronoDur::seconds(i64::from(offset)))
}

/// `local` resolved in the context zone when there is one, else kept naive.
fn local_in_zone(local: NaiveDateTime, zone: Option<&str>) -> Option<NaiveDateTime> {
    match zone {
        Some(z) => utc_in_zone(local, z),
        None => Some(local),
    }
}

fn as_utc_instant(t: &Temporal, ctx: Option<&Temporal>) -> Option<NaiveDateTime> {
    let zone = ctx.and_then(iana_zone);
    let ctx_date = ctx.and_then(wall_dt).map(|d| d.date());
    match t {
        Temporal::DateTime { utc, .. } => Some(*utc),
        Temporal::Date(d) => local_in_zone(d.and_time(NaiveTime::MIN), zone),
        Temporal::LocalDateTime(dt) => local_in_zone(*dt, zone),
        Temporal::LocalTime(tm) => {
            let date = ctx_date.or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1))?;
            let local = date.and_time(*tm);
            if let Some(z) = zone {
                utc_in_zone(local, z)
            } else if let Some(Temporal::DateTime { offset, .. }) = ctx {
                // checked_neg: a user-parsed offset can be exactly i32::MIN.
                shift_by_offset(local, offset.checked_neg()?)
            } else {
                Some(local)
            }
        }
        Temporal::Time { time, offset } => {
            let date = ctx_date.or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1))?;
            let local = date.and_time(*time);
            // checked_neg: a user-parsed offset can be exactly i32::MIN.
            shift_by_offset(local, offset.checked_neg()?)
        }
        Temporal::Duration { .. } | Temporal::CivilDate { .. } | Temporal::CivilDateTime { .. } => {
            None
        }
    }
}

fn wall_dt(t: &Temporal) -> Option<NaiveDateTime> {
    match t {
        Temporal::Date(d) => Some(d.and_time(NaiveTime::MIN)),
        Temporal::LocalDateTime(dt) => Some(*dt),
        Temporal::CivilDate { .. } | Temporal::CivilDateTime { .. } => None,
        Temporal::DateTime { utc, offset, .. } => shift_by_offset(*utc, *offset),
        _ => None,
    }
}

fn utc_dt(t: &Temporal) -> Option<NaiveDateTime> {
    match t {
        Temporal::DateTime { utc, .. } => Some(*utc),
        other => wall_dt(other),
    }
}

fn time_nanos_of(t: &Temporal, as_utc: bool) -> Option<i64> {
    match t {
        Temporal::LocalTime(tm) => Some(time_to_nanos(*tm)),
        Temporal::Time { time, offset } => {
            let n = time_to_nanos(*time);
            Some(if as_utc {
                n - i64::from(*offset) * 1_000_000_000
            } else {
                n
            })
        }
        Temporal::Date(_) => Some(0),
        Temporal::LocalDateTime(dt) => Some(time_to_nanos(dt.time())),
        Temporal::DateTime { utc, offset, .. } => {
            if as_utc {
                Some(time_to_nanos(utc.time()))
            } else {
                let local = shift_by_offset(*utc, *offset)?;
                Some(time_to_nanos(local.time()))
            }
        }
        Temporal::Duration { .. } => None,
        Temporal::CivilDate { .. } => Some(0),
        Temporal::CivilDateTime { time, .. } => Some(time_to_nanos(*time)),
    }
}

/// `t` overshot `db` when stepping away from `da`: the tentative result
/// landed on the far side of `db` in the direction of travel.
fn overshot(da: NaiveDateTime, db: NaiveDateTime, t: NaiveDateTime) -> bool {
    if db >= da {
        t > db
    } else {
        t < db
    }
}

/// Whole months/days/sub-day nanos between two in-chrono-range datetimes.
fn calendar_between(da: NaiveDateTime, db: NaiveDateTime) -> Result<(i64, i64, i128)> {
    // Chrono-range years are i32-bounded, so this difference cannot overflow.
    let months =
        i64::from(db.year() - da.year()) * 12 + i64::from(db.month()) - i64::from(da.month());
    let step = if db >= da { 1 } else { -1 };
    let mut adj_months = months;
    let tentative = add_months(da, adj_months)?;
    if overshot(da, db, tentative) {
        adj_months -= step;
    }
    let after_m = add_months(da, adj_months)?;
    let days = (db.date() - after_m.date()).num_days();
    let mut adj_days = days;
    let after_d = checked_add_dur(after_m, ChronoDur::try_days(adj_days))?;
    if overshot(da, db, after_d) {
        adj_days -= step;
    }
    let after_d = checked_add_dur(after_m, ChronoDur::try_days(adj_days))?;
    // `after_d` is within a day of `db`, so the nanosecond difference fits i64.
    let nanos = (db - after_d).num_nanoseconds().unwrap_or(0) as i128;
    Ok((adj_months, adj_days, nanos))
}

pub fn duration_between(a: &Temporal, b: &Temporal) -> Result<Temporal> {
    duration_between_unit(a, b, BetweenUnit::Full)
}

#[derive(Clone, Copy)]
pub enum BetweenUnit {
    Full,
    Months,
    Days,
    Seconds,
}

/// A civil value's parts: the unbounded i64 year, month, day, and the
/// within-day nanosecond count, named so call sites cannot swap fields.
#[derive(Clone, Copy)]
struct CivilParts {
    year: i64,
    month: u32,
    day: u32,
    nanos: i64,
}

fn civil_parts(t: &Temporal) -> Option<CivilParts> {
    match t {
        Temporal::CivilDate { year, month, day } => Some(CivilParts {
            year: *year,
            month: *month,
            day: *day,
            nanos: 0,
        }),
        Temporal::CivilDateTime {
            year,
            month,
            day,
            time,
        } => Some(CivilParts {
            year: *year,
            month: *month,
            day: *day,
            nanos: time_to_nanos(*time),
        }),
        Temporal::Date(d) => Some(CivilParts {
            year: i64::from(d.year()),
            month: d.month(),
            day: d.day(),
            nanos: 0,
        }),
        Temporal::LocalDateTime(dt) => Some(CivilParts {
            year: i64::from(dt.year()),
            month: dt.month(),
            day: dt.day(),
            nanos: time_to_nanos(dt.time()),
        }),
        _ => None,
    }
}

/// Whether `t` is a genuinely civil value (unbounded i64 years).
fn is_civil(t: &Temporal) -> bool {
    matches!(
        t,
        Temporal::CivilDate { .. } | Temporal::CivilDateTime { .. }
    )
}

/// The civil parts of both operands when they are date-like and at least one
/// is a genuinely civil value (unbounded i64 years); `None` leaves the pair
/// to the chrono/zone domains below.
fn civil_parts_pair(a: &Temporal, b: &Temporal) -> Option<(CivilParts, CivilParts)> {
    if !is_civil(a) && !is_civil(b) {
        return None;
    }
    Some((civil_parts(a)?, civil_parts(b)?))
}

/// The exact i128 difference of two civil values, shaped per `unit` and
/// range-checked into the `Duration` fields.
fn civil_duration(a: CivilParts, b: CivilParts, unit: BetweenUnit) -> Result<Temporal> {
    // Civil years are unbounded i64; compute in i128 and error when the
    // result does not fit the Duration fields.
    let months =
        (i128::from(b.year) - i128::from(a.year)) * 12 + i128::from(b.month) - i128::from(a.month);
    let months = i64::try_from(months).map_err(|_| range_err())?;
    let day_count =
        || civil_epoch_days(b.year, b.month, b.day) - civil_epoch_days(a.year, a.month, a.day);
    Ok(match unit {
        BetweenUnit::Full => shaped_duration(
            months,
            i64::try_from(i128::from(b.day) - i128::from(a.day)).map_err(|_| range_err())?,
            i128::from(b.nanos) - i128::from(a.nanos),
            BetweenUnit::Full,
        ),
        BetweenUnit::Months => shaped_duration(months, 0, 0, BetweenUnit::Months),
        BetweenUnit::Days => shaped_duration(
            0,
            i64::try_from(day_count()).map_err(|_| range_err())?,
            0,
            BetweenUnit::Days,
        ),
        // Fits i128 for any i64 year (~5.8e35 ns at ±i64::MAX years).
        BetweenUnit::Seconds => shaped_duration(
            0,
            0,
            day_count() * 86_400_000_000_000 + (i128::from(b.nanos) - i128::from(a.nanos)),
            BetweenUnit::Seconds,
        ),
    })
}

/// Shape a `(months, days, nanos)` difference per `unit`: `Full` keeps all
/// three, `Months` keeps months, `Days` keeps days, `Seconds` keeps nanos.
/// The one place the per-unit `Duration` shaping lives.
fn shaped_duration(months: i64, days: i64, nanos: i128, unit: BetweenUnit) -> Temporal {
    match unit {
        BetweenUnit::Full => Temporal::Duration {
            months,
            days,
            nanos,
        },
        BetweenUnit::Months => Temporal::Duration {
            months,
            days: 0,
            nanos: 0,
        },
        BetweenUnit::Days => Temporal::Duration {
            months: 0,
            days,
            nanos: 0,
        },
        BetweenUnit::Seconds => Temporal::Duration {
            months: 0,
            days: 0,
            nanos,
        },
    }
}

/// The one classification of the operand pair that selects the comparison
/// domain (F-1-3): every predicate is evaluated exactly once here, and
/// `duration_between_unit` matches on the outcome instead of re-deriving it.
enum PairDomain<'a> {
    /// Both operands are date-like and at least one is a genuinely civil
    /// value (unbounded i64 years): the exact i128 difference.
    Civil { a: CivilParts, b: CivilParts },
    /// At least one operand is a time of day: compare within-day nanos, read
    /// at UTC when both operands are zoned (`as_utc`). `mixed` marks the
    /// time-vs-non-time shape, which has no months or days between operands.
    TimeOnly { mixed: bool, as_utc: bool },
    /// Compare at the zone of `ctx`, the pair's first IANA-zoned operand.
    Zoned { ctx: &'a Temporal },
    /// Both operands are zoned without an IANA region: compare UTC instants.
    Instant,
    /// Neither operand is zoned: compare naive wall clocks.
    Wall,
}

/// An operand's comparison shape, classified once per operand so the pair
/// match never re-probes the value.
enum OperandShape {
    /// A time of day; `zoned` distinguishes `Temporal::Time` (an
    /// offset-carrying time) from `Temporal::LocalTime`.
    Time { zoned: bool },
    /// A `Temporal::DateTime`; `iana` is whether it names an IANA region.
    DateTime { iana: bool },
    /// A date, local datetime, civil value or duration.
    Other,
}

fn operand_shape(t: &Temporal) -> OperandShape {
    match t {
        Temporal::LocalTime(_) => OperandShape::Time { zoned: false },
        Temporal::Time { .. } => OperandShape::Time { zoned: true },
        Temporal::DateTime { .. } => OperandShape::DateTime {
            iana: iana_zone(t).is_some(),
        },
        Temporal::Date(_)
        | Temporal::CivilDate { .. }
        | Temporal::LocalDateTime(_)
        | Temporal::CivilDateTime { .. }
        | Temporal::Duration { .. } => OperandShape::Other,
    }
}

fn pair_domain<'a>(a: &'a Temporal, b: &'a Temporal) -> PairDomain<'a> {
    if let Some((a_parts, b_parts)) = civil_parts_pair(a, b) {
        return PairDomain::Civil {
            a: a_parts,
            b: b_parts,
        };
    }
    use OperandShape::*;
    match (operand_shape(a), operand_shape(b)) {
        // Both time-of-day: within-day nanos, read at UTC when both zoned.
        (Time { zoned: za }, Time { zoned: zb }) => PairDomain::TimeOnly {
            mixed: false,
            as_utc: za && zb,
        },
        // A time of day against a non-IANA datetime: no months or days
        // between them; the nanos are read at UTC when the time side is
        // `Temporal::Time` (the datetime side is always zoned).
        (Time { zoned }, DateTime { iana: false }) | (DateTime { iana: false }, Time { zoned }) => {
            PairDomain::TimeOnly {
                mixed: true,
                as_utc: zoned,
            }
        }
        // A time of day against a date-like value: same, never at UTC.
        (Time { .. }, Other) | (Other, Time { .. }) => PairDomain::TimeOnly {
            mixed: true,
            as_utc: false,
        },
        // An IANA-zoned datetime reads the pair at its zone; the first
        // operand wins when both name one.
        (DateTime { iana: true }, _) => PairDomain::Zoned { ctx: a },
        (_, DateTime { iana: true }) => PairDomain::Zoned { ctx: b },
        // Both sides zoned without an IANA region: compare UTC instants.
        (DateTime { iana: false }, DateTime { iana: false }) => PairDomain::Instant,
        // The remaining pairs hold at most one zoned side: wall clocks.
        (DateTime { iana: false }, Other) | (Other, DateTime { iana: false }) | (Other, Other) => {
            PairDomain::Wall
        }
    }
}

/// Both operands resolved by `resolve`, or the `duration.between` type error
/// when a value has no reading in the pair's domain.
fn resolve_pair(
    a: &Temporal,
    b: &Temporal,
    resolve: impl Fn(&Temporal) -> Option<NaiveDateTime>,
) -> Result<(NaiveDateTime, NaiveDateTime)> {
    let resolve_one = |t: &Temporal| resolve(t).ok_or_else(|| Error::ty("duration.between"));
    Ok((resolve_one(a)?, resolve_one(b)?))
}

/// Shape the difference between the pair's two resolved datetimes per `unit`.
fn calendar_duration(da: NaiveDateTime, db: NaiveDateTime, unit: BetweenUnit) -> Result<Temporal> {
    match unit {
        BetweenUnit::Full => {
            let (months, days, nanos) = calendar_between(da, db)?;
            Ok(shaped_duration(months, days, nanos, BetweenUnit::Full))
        }
        BetweenUnit::Months => {
            let (months, _, _) = calendar_between(da, db)?;
            Ok(shaped_duration(months, 0, 0, BetweenUnit::Months))
        }
        BetweenUnit::Days => {
            let days = (db - da).num_days();
            Ok(shaped_duration(0, days, 0, BetweenUnit::Days))
        }
        BetweenUnit::Seconds => {
            // Two chrono-range datetimes can differ by more than i64
            // nanoseconds (±262143 years ≈ ±8.8e21 ns), where
            // `TimeDelta::num_nanoseconds` returns `None`; the old
            // `unwrap_or(0)` silently reported PT0S. Compute in i128 from
            // whole days plus within-day nanos instead.
            let day_ns = i128::from((db.date() - da.date()).num_days()) * 86_400_000_000_000;
            let nanos = day_ns + i128::from(time_to_nanos(db.time()))
                - i128::from(time_to_nanos(da.time()));
            Ok(shaped_duration(0, 0, nanos, BetweenUnit::Seconds))
        }
    }
}

pub fn duration_between_unit(a: &Temporal, b: &Temporal, unit: BetweenUnit) -> Result<Temporal> {
    match pair_domain(a, b) {
        PairDomain::Civil {
            a: a_parts,
            b: b_parts,
        } => civil_duration(a_parts, b_parts, unit),
        PairDomain::TimeOnly { mixed, as_utc } => {
            if mixed && matches!(unit, BetweenUnit::Months | BetweenUnit::Days) {
                return Ok(Temporal::Duration {
                    months: 0,
                    days: 0,
                    nanos: 0,
                });
            }
            let na = time_nanos_of(a, as_utc).ok_or_else(|| Error::ty("duration.between"))?;
            let nb = time_nanos_of(b, as_utc).ok_or_else(|| Error::ty("duration.between"))?;
            Ok(dur_from_nanos(nb - na, unit))
        }
        PairDomain::Zoned { ctx } => {
            let (da, db) = resolve_pair(a, b, |t| as_utc_instant(t, Some(ctx)))?;
            calendar_duration(da, db, unit)
        }
        PairDomain::Instant => {
            let (da, db) = resolve_pair(a, b, utc_dt)?;
            calendar_duration(da, db, unit)
        }
        PairDomain::Wall => {
            let (da, db) = resolve_pair(a, b, wall_dt)?;
            calendar_duration(da, db, unit)
        }
    }
}

fn dur_from_nanos(nanos: i64, unit: BetweenUnit) -> Temporal {
    shaped_duration(0, 0, i128::from(nanos), unit)
}

fn add_months(dt: NaiveDateTime, months: i64) -> Result<NaiveDateTime> {
    // Total month index over the proleptic calendar, then one euclidean split.
    // `months` is user-controlled, so the sum is checked: an i64 overflow here
    // would otherwise reach the range check only through a wrap.
    let total = (i64::from(dt.year()) * 12 + (i64::from(dt.month()) - 1))
        .checked_add(months)
        .ok_or_else(range_err)?;
    let year = i32::try_from(total.div_euclid(12)).map_err(|_| range_err())?;
    let month = u32::try_from(total.rem_euclid(12) + 1).map_err(|_| range_err())?;
    let last = last_day(year, month);
    let day = dt.day().min(last);
    let date = NaiveDate::from_ymd_opt(year, month, day).ok_or_else(range_err)?;
    Ok(date.and_time(dt.time()))
}

fn last_day(year: i32, month: u32) -> u32 {
    NaiveDate::from_ymd_opt(year, month, 1)
        .and_then(|d| {
            let next = if month == 12 {
                year.checked_add(1)
                    .and_then(|y| NaiveDate::from_ymd_opt(y, 1, 1))
            } else {
                NaiveDate::from_ymd_opt(year, month + 1, 1)
            };
            next.map(|n| (n - d).num_days() as u32)
        })
        .unwrap_or(28)
}

pub fn add_duration(t: &Temporal, d: &Temporal) -> Result<Temporal> {
    let Temporal::Duration {
        months,
        days,
        nanos,
    } = d
    else {
        return Err(Error::ty("expected duration"));
    };
    match t {
        Temporal::Duration {
            months: m,
            days: da,
            nanos: n,
        } => Ok(Temporal::Duration {
            months: m.checked_add(*months).ok_or_else(range_err)?,
            days: da.checked_add(*days).ok_or_else(range_err)?,
            nanos: n.checked_add(*nanos).ok_or_else(range_err)?,
        }),
        Temporal::Date(date) => {
            // Fractional months/hours overflow into days; include that in date arithmetic.
            // Truncate toward zero so a negative time component smaller than one day
            // does not steal an extra calendar day. The i128 day count may exceed i64
            // for extreme durations; error honestly instead of wrapping through `as`.
            let extra_days = i64::try_from(*nanos / 86_400_000_000_000).map_err(|_| range_err())?;
            let days_total = days.checked_add(extra_days).ok_or_else(range_err)?;
            let base = add_months(date.and_time(NaiveTime::MIN), *months)?;
            let dt = checked_add_dur(base, ChronoDur::try_days(days_total))?;
            Ok(Temporal::Date(dt.date()))
        }
        Temporal::LocalDateTime(dt) => {
            let base = add_months(*dt, *months)?;
            let base = checked_add_dur(base, ChronoDur::try_days(*days))?;
            let nanos64 = i64::try_from(*nanos).map_err(|_| range_err())?;
            Ok(Temporal::LocalDateTime(checked_add_dur(
                base,
                Some(ChronoDur::nanoseconds(nanos64)),
            )?))
        }
        Temporal::DateTime { utc, offset, zone } => {
            let local = add_offset(*utc, *offset)?;
            let local = add_months(local, *months)?;
            let local = checked_add_dur(local, ChronoDur::try_days(*days))?;
            let nanos64 = i64::try_from(*nanos).map_err(|_| range_err())?;
            let local = checked_add_dur(local, Some(ChronoDur::nanoseconds(nanos64)))?;
            let utc = sub_offset(local, *offset)?;
            Ok(Temporal::DateTime {
                utc,
                offset: *offset,
                zone: zone.clone(),
            })
        }
        Temporal::LocalTime(tm) => {
            let n = (time_to_nanos(*tm) as i128 + *nanos).rem_euclid(86_400_000_000_000) as i64;
            Ok(Temporal::LocalTime(nanos_to_time(n)))
        }
        Temporal::Time { time, offset } => {
            let n = (time_to_nanos(*time) as i128 + *nanos).rem_euclid(86_400_000_000_000) as i64;
            Ok(Temporal::Time {
                time: nanos_to_time(n),
                offset: *offset,
            })
        }
        Temporal::CivilDate { .. } | Temporal::CivilDateTime { .. } => {
            Err(Error::ty("civil date arithmetic out of range"))
        }
    }
}

pub fn sub_duration(t: &Temporal, d: &Temporal) -> Result<Temporal> {
    let Temporal::Duration {
        months,
        days,
        nanos,
    } = d
    else {
        return Err(Error::ty("expected duration"));
    };
    add_duration(
        t,
        &Temporal::Duration {
            months: months.checked_neg().ok_or_else(range_err)?,
            days: days.checked_neg().ok_or_else(range_err)?,
            nanos: nanos.checked_neg().ok_or_else(range_err)?,
        },
    )
}

/// Scale a duration by a number (openCypher `duration * n` / `duration / n`).
pub fn scale_duration(t: &Temporal, factor: f64) -> Result<Temporal> {
    let Temporal::Duration {
        months,
        days,
        nanos,
    } = t
    else {
        return Err(Error::ty("scale duration"));
    };
    if !factor.is_finite() {
        return Err(Error::ty("duration scale factor"));
    }
    let m = *months as f64 * factor;
    let d = *days as f64 * factor;
    let n = *nanos as f64 * factor;
    let months_out = m.trunc();
    let days_total = d + (m - months_out) * AVG_DAYS_PER_MONTH;
    let days_out = days_total.trunc();
    let nanos_out = n + (days_total - days_out) * NANOS_PER_DAY;
    Ok(Temporal::Duration {
        months: super::f64_to_i64_range(months_out)?,
        days: super::f64_to_i64_range(days_out)?,
        nanos: super::f64_to_i128_range(nanos_out.trunc())?,
    })
}
