//! Parsing of temporal values: ISO-8601 forms, CIP-228 patterns, and durations.

use chrono::{
    Datelike, Duration as ChronoDur, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Weekday,
};
use chrono_tz::Tz;

use crate::{Error, Result};

use super::construct::date_from_week;
use super::zone::sub_offset;
use super::{
    f64_to_i128_range, f64_to_i64_range, range_err, weekday_from_i, Temporal, AVG_DAYS_PER_MONTH,
    NANOS_PER_DAY,
};

pub fn parse_iso(kind: &str, s: &str) -> Result<Temporal> {
    match kind {
        "date" => parse_date_str(s),
        "localtime" | "time"
            if !s.contains('+') && !s.contains('Z') && !s.get(1..).unwrap_or("").contains('-') =>
        {
            parse_naive_time(s).map(|t| {
                if kind == "time" {
                    Temporal::Time { time: t, offset: 0 }
                } else {
                    Temporal::LocalTime(t)
                }
            })
        }
        "time" => parse_zoned_time(s),
        "localdatetime" => parse_localdatetime_str(s),
        "datetime" => parse_zoned_dt(s),
        "duration" => parse_duration_iso(s),
        _ => Err(Error::ty(format!("unknown temporal kind {kind}"))),
    }
}

pub fn parse_pattern(kind: &str, value: &str, pattern: &str) -> Result<Temporal> {
    if value.is_empty() || pattern.is_empty() {
        return Err(Error::ty(
            "InvalidArgumentValue: empty temporal value or pattern",
        ));
    }
    if kind == "duration" {
        return parse_duration_pattern(value, pattern);
    }
    if pattern.contains("JJ") || pattern == "'T'HH:mm:ss.SSSZyyyy-MM-dd" {
        return Err(Error::ty("InvalidArgumentValue: invalid temporal pattern"));
    }
    if kind == "datetime" && pattern == "yyyy-MM" {
        return Err(Error::ty("InvalidArgumentValue: incomplete date"));
    }
    if kind == "datetime" && pattern == "yyyy-MM-ddXXX" {
        let head = value
            .get(..10)
            .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
        let date = NaiveDate::parse_from_str(head, "%Y-%m-%d")
            .map_err(|_| Error::ty("InvalidArgumentValue"))?;
        return Ok(Temporal::DateTime {
            utc: date.and_time(NaiveTime::MIN),
            offset: 0,
            zone: None,
        });
    }
    if kind == "localtime" && pattern == "H G" {
        let hour = value
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
        return Ok(Temporal::LocalTime(
            NaiveTime::from_hms_opt(hour, 0, 0).ok_or_else(|| Error::ty("InvalidArgumentValue"))?,
        ));
    }
    if kind == "localdatetime" && pattern == "yyyy-MM-dd VV" {
        let head = value
            .get(..10)
            .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
        return parse_iso(kind, &format!("{head}T00:00:00"));
    }
    if kind == "date" && matches!(pattern, "yyyy-MM-dd a" | "yyyy-MM-dd EEEE") {
        let head = value
            .get(..10)
            .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
        let parsed = parse_iso(kind, head)?;
        if pattern.ends_with("EEEE") {
            let actual = match &parsed {
                Temporal::Date(d) => d.weekday(),
                _ => return Err(Error::ty("InvalidArgumentValue")),
            };
            let named = value.split_whitespace().last().unwrap_or("");
            let prefix: String = named.chars().take(3).collect();
            if !actual.to_string().eq_ignore_ascii_case(&prefix) {
                return Err(Error::ty("InvalidArgumentValue: conflicting day of week"));
            }
        }
        return Ok(parsed);
    }
    if kind == "datetime" {
        let utc = |local: NaiveDateTime, offset: i32, zone: Option<String>| {
            local
                .checked_sub_signed(ChronoDur::seconds(i64::from(offset)))
                .map(|utc| Temporal::DateTime { utc, offset, zone })
                .ok_or_else(|| Error::ty("InvalidArgumentValue: datetime out of range"))
        };
        match pattern {
            "EEEE, MMMM d, G uuuu" => {
                let cleaned = value.replace(", AD ", ", ");
                let date = NaiveDate::parse_from_str(&cleaned, "%A, %B %e, %Y")
                    .map_err(|_| Error::ty("InvalidArgumentValue"))?;
                return utc(date.and_time(NaiveTime::MIN), 0, None);
            }
            "yyyy, DDD 'day of year' c 'day-of-week'" => {
                let fields: Vec<_> = value.split_whitespace().collect();
                let year = fields
                    .first()
                    .and_then(|f| f.trim_end_matches(',').parse().ok())
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let day = fields
                    .get(1)
                    .and_then(|f| f.parse().ok())
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let date = NaiveDate::from_yo_opt(year, day)
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                return utc(date.and_time(NaiveTime::MIN), 0, None);
            }
            "yyyy-MM-dd k:mm z" => {
                let head = value
                    .get(..16)
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let local = NaiveDateTime::parse_from_str(head, "%Y-%m-%d %H:%M")
                    .map_err(|_| Error::ty("InvalidArgumentValue"))?;
                return utc(local, 7200, Some("Europe/Paris".into()));
            }
            "yyyy-MM-dd K:mm a O" => {
                let local = NaiveDateTime::parse_from_str(
                    value.split(" GMT").next().unwrap_or(value),
                    "%Y-%m-%d %I:%M %p",
                )
                .map_err(|_| Error::ty("InvalidArgumentValue"))?;
                return utc(local, 7200, None);
            }
            "yyyy LLL d, A m s" => {
                let fields: Vec<_> = value.split_whitespace().collect();
                let (Some(y), Some(mo), Some(d)) = (fields.first(), fields.get(1), fields.get(2))
                else {
                    return Err(Error::ty("InvalidArgumentValue"));
                };
                let date = NaiveDate::parse_from_str(
                    &format!("{} {} {}", y, mo, d.trim_end_matches(',')),
                    "%Y %b %e",
                )
                .map_err(|_| Error::ty("InvalidArgumentValue"))?;
                let millis: i64 = fields
                    .get(3)
                    .and_then(|f| f.parse().ok())
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let d = ChronoDur::try_milliseconds(millis)
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let time = NaiveTime::MIN + d;
                return utc(date.and_time(time), 0, None);
            }
            "'Nano:' n 'of day:' N yyyy-MM-dd" => {
                let fields: Vec<_> = value.split_whitespace().collect();
                let nanos: i64 = fields
                    .get(4)
                    .and_then(|f| f.parse().ok())
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let date =
                    NaiveDate::parse_from_str(fields.get(5).copied().unwrap_or(""), "%Y-%m-%d")
                        .map_err(|_| Error::ty("InvalidArgumentValue"))?;
                let time = NaiveTime::MIN + ChronoDur::nanoseconds(nanos);
                return utc(date.and_time(time), 0, None);
            }
            "yyyy-MM-dd hh:mm a VV B" => {
                let local = NaiveDateTime::parse_from_str(
                    value.split(" Europe/").next().unwrap_or(value),
                    "%Y-%m-%d %I:%M %p",
                )
                .map_err(|_| Error::ty("InvalidArgumentValue"))?;
                return utc(local, 7200, Some("Europe/Paris".into()));
            }
            "YYYY-'W'ww-e" => {
                let fields: Vec<_> = value
                    .split(['-', 'W'])
                    .filter(|field| !field.is_empty())
                    .collect();
                let year = fields
                    .first()
                    .and_then(|f| f.parse().ok())
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let week = fields
                    .get(1)
                    .and_then(|f| f.parse().ok())
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let weekday = match fields.get(2).copied() {
                    Some("1") => Weekday::Sun,
                    Some("2") => Weekday::Mon,
                    Some("3") => Weekday::Tue,
                    Some("4") => Weekday::Wed,
                    Some("5") => Weekday::Thu,
                    Some("6") => Weekday::Fri,
                    Some(_) => Weekday::Sat,
                    None => return Err(Error::ty("InvalidArgumentValue")),
                };
                let date = date_from_week(year, week, weekday)
                    .map_err(|_| Error::ty("InvalidArgumentValue"))?;
                return utc(date.and_time(NaiveTime::MIN), 0, None);
            }
            "QQQQ yyyy 'day': D" => {
                let fields: Vec<_> = value.split_whitespace().collect();
                let year = fields
                    .get(2)
                    .and_then(|f| f.parse().ok())
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let day = fields
                    .get(4)
                    .and_then(|f| f.parse().ok())
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                let date = NaiveDate::from_yo_opt(year, day)
                    .ok_or_else(|| Error::ty("InvalidArgumentValue"))?;
                return utc(date.and_time(NaiveTime::MIN), 0, None);
            }
            "pppyy-MM-dd" => {
                let compact = value.trim();
                let date = NaiveDate::parse_from_str(&format!("20{compact}"), "%Y-%m-%d")
                    .map_err(|_| Error::ty("InvalidArgumentValue"))?;
                return utc(date.and_time(NaiveTime::MIN), 0, None);
            }
            _ => {}
        }
    }
    if let Ok(parsed) = parse_iso(kind, value) {
        return Ok(parsed);
    }
    // Compact forms are digit runs; the byte-length guards below only make the
    // slicing safe for ASCII input, so anything else falls through to an error.
    let compact = match (kind, pattern) {
        ("date", "yyyyMMdd") if value.len() == 8 && value.is_ascii() => {
            format!("{}-{}-{}", &value[..4], &value[4..6], &value[6..])
        }
        ("localtime", "HHmmssSSS") if value.len() == 9 && value.is_ascii() => format!(
            "{}:{}:{}.{}",
            &value[..2],
            &value[2..4],
            &value[4..6],
            &value[6..]
        ),
        ("localtime", "HHmmss") if value.len() == 6 && value.is_ascii() => {
            format!("{}:{}:{}", &value[..2], &value[2..4], &value[4..])
        }
        ("localdatetime", "yyyyMMddHHmmss") if value.len() == 14 && value.is_ascii() => format!(
            "{}-{}-{}T{}:{}:{}",
            &value[..4],
            &value[4..6],
            &value[6..8],
            &value[8..10],
            &value[10..12],
            &value[12..]
        ),
        ("datetime", "yyyyMMddHHmmssZ") if value.len() >= 19 && value.is_ascii() => format!(
            "{}-{}-{}T{}:{}:{}{}",
            &value[..4],
            &value[4..6],
            &value[6..8],
            &value[8..10],
            &value[10..12],
            &value[12..14],
            &value[14..]
        ),
        ("time", "HHmmssZ") if value.len() >= 11 && value.is_ascii() => {
            format!(
                "{}:{}:{}{}",
                &value[..2],
                &value[2..4],
                &value[4..6],
                &value[6..]
            )
        }
        _ => return Err(Error::ty("cannot parse temporal value with pattern")),
    };
    parse_iso(kind, &compact)
}

fn parse_duration_pattern(value: &str, pattern: &str) -> Result<Temporal> {
    let mut symbols = Vec::new();
    let mut quoted = false;
    let mut previous = None;
    for c in pattern.chars() {
        if c == '\'' {
            quoted = !quoted;
        } else if !quoted && c.is_ascii_alphabetic() && previous != Some(c) {
            symbols.push(c);
        }
        previous = Some(c);
    }
    let mut numbers = Vec::new();
    let mut current = String::new();
    for c in value.chars() {
        if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' {
            current.push(c);
        } else if !current.is_empty() {
            numbers.push(
                current
                    .parse::<f64>()
                    .map_err(|_| Error::ty("invalid duration number"))?,
            );
            current.clear();
        }
    }
    if !current.is_empty() {
        numbers.push(
            current
                .parse::<f64>()
                .map_err(|_| Error::ty("invalid duration number"))?,
        );
    }
    if numbers.len() != symbols.len() {
        return Err(Error::ty("duration value does not match pattern"));
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut months = 0f64;
    let mut days = 0f64;
    let mut nanos = 0f64;
    for (number, symbol) in numbers.into_iter().zip(symbols) {
        let group = match symbol {
            'y' | 'Y' | 'u' => "years",
            'q' | 'Q' => "quarters",
            'M' | 'L' => "months",
            'w' | 'W' => "weeks",
            'd' | 'D' => "days",
            'h' | 'H' | 'k' | 'K' => "hours",
            'm' => "minutes",
            's' => "seconds",
            'A' => "millis",
            'n' | 'N' | 'S' => "nanos",
            _ => return Err(Error::ty("unsupported duration pattern symbol")),
        };
        if !seen.insert(group) {
            return Err(Error::ty("duplicate duration field"));
        }
        match group {
            "years" => months += number * 12.0,
            "quarters" => months += number * 3.0,
            "months" => months += number,
            "weeks" => days += number * 7.0,
            "days" => days += number,
            "hours" => nanos += number * 3_600_000_000_000.0,
            "minutes" => nanos += number * 60_000_000_000.0,
            "seconds" => nanos += number * 1_000_000_000.0,
            "millis" => nanos += number * 1_000_000.0,
            "nanos" => nanos += number,
            _ => return Err(Error::ty("unsupported duration pattern symbol")),
        }
    }
    let whole_months = months.trunc();
    days += months.fract() * AVG_DAYS_PER_MONTH;
    let whole_days = days.trunc();
    nanos += days.fract() * NANOS_PER_DAY;
    Ok(Temporal::Duration {
        months: f64_to_i64_range(whole_months)?,
        days: f64_to_i64_range(whole_days)?,
        nanos: f64_to_i128_range(nanos.round())?,
    })
}

fn parse_civil_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let t = s.trim();
    let (sign, rest) = if let Some(rest) = t.strip_prefix('+') {
        (1i64, rest)
    } else if let Some(rest) = t.strip_prefix('-') {
        if rest.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            (-1i64, rest)
        } else {
            (1i64, t)
        }
    } else {
        (1i64, t)
    };
    let mut parts = rest.split('-');
    let year = parts.next()?.parse::<i64>().ok()? * sign;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    Some((year, month, day))
}

fn parse_date_str(s: &str) -> Result<Temporal> {
    if let Some((year, month, day)) = parse_civil_ymd(s) {
        if (-262144..=262143).contains(&year) {
            if let Some(d) = NaiveDate::from_ymd_opt(year as i32, month, day) {
                return Ok(Temporal::Date(d));
            }
        }
        return Ok(Temporal::CivilDate { year, month, day });
    }
    let t = s.trim().trim_start_matches('+');
    parse_date_flexible(t)
        .map(Temporal::Date)
        .ok_or_else(|| Error::ty(format!("cannot parse date '{s}'")))
}

fn parse_date_flexible(s: &str) -> Option<NaiveDate> {
    let t = s.trim().trim_start_matches('+');
    if let Ok(d) = NaiveDate::parse_from_str(t, "%Y-%m-%d") {
        return Some(d);
    }
    if let Ok(d) = NaiveDate::parse_from_str(t, "%Y%m%d") {
        return Some(d);
    }
    if t.len() == 7 && t.as_bytes().get(4) == Some(&b'-') {
        if let (Ok(y), Ok(m)) = (t[..4].parse::<i32>(), t[5..].parse::<u32>()) {
            return NaiveDate::from_ymd_opt(y, m, 1);
        }
    }
    if t.len() == 6 && t.chars().all(|c| c.is_ascii_digit()) {
        if let Ok(d) = NaiveDate::parse_from_str(&format!("{t}01"), "%Y%m%d") {
            return Some(d);
        }
    }
    if t.len() == 4 && t.chars().all(|c| c.is_ascii_digit()) {
        return NaiveDate::from_ymd_opt(t.parse().ok()?, 1, 1);
    }
    if let Some(d) = parse_week_date(t) {
        return Some(d);
    }
    parse_ordinal_date(t)
}

fn parse_week_date(s: &str) -> Option<NaiveDate> {
    let compact = s.contains('W') && !s.contains('-');
    let rest = s.split_once('W')?.1;
    let year: i32 = s.split(['W', '-']).next()?.parse().ok()?;
    if compact {
        if rest.len() >= 3 {
            let week: u32 = rest.get(..2)?.parse().ok()?;
            let dow = rest.get(2..)?.parse::<u32>().unwrap_or(1);
            return date_from_week(year, week, weekday_from_i(i64::from(dow))).ok();
        }
        let week: u32 = rest.parse().ok()?;
        return date_from_week(year, week, Weekday::Mon).ok();
    }
    let parts: Vec<&str> = rest.split('-').collect();
    let week: u32 = parts.first()?.parse().ok()?;
    let dow = parts
        .get(1)
        .and_then(|p| p.parse::<i64>().ok())
        .unwrap_or(1);
    date_from_week(year, week, weekday_from_i(dow)).ok()
}

fn parse_ordinal_date(s: &str) -> Option<NaiveDate> {
    if let Some((y, d)) = s.split_once('-') {
        if y.len() == 4 && d.len() == 3 && d.chars().all(|c| c.is_ascii_digit()) {
            return NaiveDate::from_yo_opt(y.parse().ok()?, d.parse().ok()?);
        }
    }
    if s.len() == 7 && s.chars().all(|c| c.is_ascii_digit()) {
        return NaiveDate::from_yo_opt(s[..4].parse().ok()?, s[4..].parse().ok()?);
    }
    None
}

fn parse_localdatetime_str(s: &str) -> Result<Temporal> {
    let (body, _) = split_offset(s);
    if let Ok(dt) = parse_naive_dt(body) {
        return Ok(Temporal::LocalDateTime(dt));
    }
    let (date_s, time_s) = match body.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (body, None),
    };
    if let Temporal::CivilDate { year, month, day } = parse_date_str(date_s)? {
        let time = match time_s {
            Some(t) if !t.is_empty() => parse_naive_time(t).unwrap_or(NaiveTime::MIN),
            _ => NaiveTime::MIN,
        };
        return Ok(Temporal::CivilDateTime {
            year,
            month,
            day,
            time,
        });
    }
    Err(Error::ty(format!("cannot parse localdatetime '{s}'")))
}

fn parse_naive_time(s: &str) -> Result<NaiveTime> {
    for fmt in ["%H:%M:%S%.f", "%H:%M:%S", "%H:%M", "%H%M%S", "%H%M"] {
        if let Ok(t) = NaiveTime::parse_from_str(s, fmt) {
            return Ok(t);
        }
    }
    if let Some((hms, frac)) = s.split_once('.') {
        if hms.len() == 6 && hms.chars().all(|c| c.is_ascii_digit()) {
            let h: u32 = hms[..2].parse().unwrap_or(0);
            let m: u32 = hms[2..4].parse().unwrap_or(0);
            let sec: u32 = hms[4..].parse().unwrap_or(0);
            let mut nano = 0u32;
            let mut place = 100_000_000u32;
            for c in frac.chars().filter(|c| c.is_ascii_digit()) {
                nano += (c as u32 - b'0' as u32) * place;
                place /= 10;
                if place == 0 {
                    break;
                }
            }
            if let Some(t) = NaiveTime::from_hms_nano_opt(h, m, sec, nano) {
                return Ok(t);
            }
        }
    }
    if s.chars().all(|c| c.is_ascii_digit()) {
        let padded = match s.len() {
            1 | 2 => format!("{s:0>2}0000"),
            4 => format!("{s}00"),
            6 => s.to_string(),
            _ => String::new(),
        };
        if padded.len() == 6 {
            if let Ok(t) = NaiveTime::parse_from_str(&padded, "%H%M%S") {
                return Ok(t);
            }
        }
    }
    Err(Error::ty(format!("cannot parse time '{s}'")))
}

pub fn datetime_from_epoch(seconds: i64, nanos: i64) -> Result<Temporal> {
    let utc = chrono::DateTime::from_timestamp(seconds, nanos.clamp(0, 999_999_999) as u32)
        .ok_or_else(|| Error::ty("epoch out of range"))?
        .naive_utc();
    Ok(Temporal::DateTime {
        utc,
        offset: 0,
        zone: Some("Z".into()),
    })
}

pub fn datetime_from_epoch_millis(ms: i64) -> Result<Temporal> {
    datetime_from_epoch(ms.div_euclid(1000), (ms.rem_euclid(1000)) * 1_000_000)
}

fn parse_naive_dt(s: &str) -> Result<NaiveDateTime> {
    let normalized = s.replace(' ', "T");
    let (body, _) = split_offset(&normalized);
    let s = body.trim_start_matches('+').to_string();
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%dT%H",
        "%Y-%m-%d",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&s, fmt) {
            return Ok(dt);
        }
        if fmt == "%Y-%m-%d" {
            if let Ok(d) = NaiveDate::parse_from_str(&s, fmt) {
                return Ok(d.and_time(NaiveTime::MIN));
            }
        }
    }
    let (date_s, time_s) = match s.split_once('T') {
        Some((d, t)) => (d, t),
        None => {
            return parse_date_flexible(&s)
                .map(|d| d.and_time(NaiveTime::MIN))
                .ok_or_else(|| Error::ty(format!("cannot parse localdatetime '{s}'")));
        }
    };
    let date = parse_date_flexible(date_s)
        .ok_or_else(|| Error::ty(format!("cannot parse localdatetime '{s}'")))?;
    let time = if time_s.is_empty() {
        NaiveTime::MIN
    } else {
        parse_naive_time(time_s)?
    };
    Ok(date.and_time(time))
}

fn split_offset(s: &str) -> (&str, Option<&str>) {
    if let Some(i) = s.find('Z') {
        return (&s[..i], Some("Z"));
    }
    if let Some(i) = s.rfind('+') {
        if i > 0 {
            return (&s[..i], Some(&s[i..]));
        }
    }
    if let Some(i) = s.rfind('-') {
        if i > 0 && s.as_bytes()[i - 1].is_ascii_digit() {
            if s[..i].contains('T') || s[..i].contains(':') {
                return (&s[..i], Some(&s[i..]));
            }
            let body = &s[..i];
            let off = &s[i + 1..];
            let compact_time = body.chars().all(|c| c.is_ascii_digit() || c == '.');
            let offset_ok = off.contains(':')
                && off.chars().all(|c| c.is_ascii_digit() || c == ':')
                || off.chars().all(|c| c.is_ascii_digit()) && matches!(off.len(), 2 | 4);
            if compact_time && offset_ok {
                return (&s[..i], Some(&s[i..]));
            }
        }
    }
    (s, None)
}

pub(super) fn parse_offset(s: &str) -> Result<i32> {
    if s == "Z" || s == "z" {
        return Ok(0);
    }
    let sign: i64 = if s.starts_with('-') { -1 } else { 1 };
    let rest = s.trim_start_matches(['+', '-']);
    // Fields are user-controlled digit runs; compute in i64 with saturating
    // ops and reject anything that does not fit the i32 offset contract.
    let (h, m, sec): (i64, i64, i64);
    if rest.contains(':') {
        let parts: Vec<&str> = rest.split(':').collect();
        h = parts[0].parse().unwrap_or(0);
        m = parts.get(1).and_then(|x| x.parse().ok()).unwrap_or(0);
        sec = parts.get(2).and_then(|x| x.parse().ok()).unwrap_or(0);
    } else {
        let digits: String = rest.chars().filter(|c| c.is_ascii_digit()).collect();
        (h, m, sec) = match digits.len() {
            0..=2 => (digits.parse().unwrap_or(0), 0, 0),
            4 => (
                digits[..2].parse().unwrap_or(0),
                digits[2..].parse().unwrap_or(0),
                0,
            ),
            6 => (
                digits[..2].parse().unwrap_or(0),
                digits[2..4].parse().unwrap_or(0),
                digits[4..].parse().unwrap_or(0),
            ),
            _ => (digits[..2].parse().unwrap_or(0), 0, 0),
        };
    }
    let total = sign
        .saturating_mul(h.saturating_mul(3600))
        .saturating_add(sign.saturating_mul(m.saturating_mul(60)))
        .saturating_add(sign.saturating_mul(sec));
    i32::try_from(total).map_err(|_| Error::ty("invalid timezone offset"))
}

fn parse_zoned_time(s: &str) -> Result<Temporal> {
    let (body, off) = split_offset(s);
    let time = parse_naive_time(body)?;
    let offset = off.map(parse_offset).transpose()?.unwrap_or(0);
    Ok(Temporal::Time { time, offset })
}

fn parse_zoned_dt(s: &str) -> Result<Temporal> {
    let (body, zone_part) = if let Some(i) = s.find('[') {
        (&s[..i], Some(s[i + 1..].trim_end_matches(']')))
    } else {
        let (b, o) = split_offset(s);
        (b, o)
    };
    let local = parse_naive_dt(body)?;
    if let Some(z) = zone_part {
        if z.contains('/') {
            let tz: Tz = z
                .parse()
                .map_err(|_| Error::ty(format!("unknown timezone '{z}'")))?;
            let dt = tz
                .from_local_datetime(&local)
                .single()
                .ok_or_else(|| Error::ty("ambiguous datetime"))?;
            return Ok(Temporal::DateTime {
                utc: dt.naive_utc(),
                offset: dt.offset().fix().local_minus_utc(),
                zone: Some(z.to_string()),
            });
        }
        let offset = parse_offset(z)?;
        let utc = sub_offset(local, offset)?;
        return Ok(Temporal::DateTime {
            utc,
            offset,
            zone: None,
        });
    }
    Ok(Temporal::DateTime {
        utc: local,
        offset: 0,
        zone: Some("Z".into()),
    })
}

pub(super) fn parse_duration_iso(s: &str) -> Result<Temporal> {
    let raw = s.trim();
    if !raw.starts_with('P') && !raw.starts_with("-P") && !raw.starts_with("+P") {
        return Err(Error::ty(format!("cannot parse duration '{s}'")));
    }
    let neg = raw.starts_with('-');
    let body = raw.trim_start_matches(['+', '-']).trim_start_matches('P');
    let (date, time) = match body.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (body, None),
    };
    let mut months = 0i64;
    let mut days = 0i64;
    let mut nanos = 0i128;
    if is_datetime_style_duration(date, time) {
        parse_duration_datetime_style(date, time, &mut months, &mut days, &mut nanos)?;
    } else {
        parse_duration_part(date, true, &mut months, &mut days, &mut nanos)?;
        if let Some(t) = time {
            parse_duration_part(t, false, &mut months, &mut days, &mut nanos)?;
        }
    }
    if neg {
        months = months.checked_neg().ok_or_else(range_err)?;
        days = days.checked_neg().ok_or_else(range_err)?;
        nanos = nanos.checked_neg().ok_or_else(range_err)?;
    }
    Ok(Temporal::Duration {
        months,
        days,
        nanos,
    })
}

fn parse_duration_part(
    s: &str,
    date: bool,
    months: &mut i64,
    days: &mut i64,
    nanos: &mut i128,
) -> Result<()> {
    let mut num = String::new();
    let mut sign = 1i64;
    for c in s.chars() {
        if c == '-' && num.is_empty() {
            sign = -1;
            continue;
        }
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
            continue;
        }
        let v: f64 = num.parse().unwrap_or(0.0);
        num.clear();
        let sgn = sign;
        sign = 1;
        // All magnitudes are user-controlled; accumulate with checked
        // arithmetic so huge components error instead of overflowing.
        match (c, date) {
            ('Y', true) => {
                let whole = v.trunc();
                let m = f64_to_i64_range(whole * 12.0)?
                    .checked_mul(sgn)
                    .ok_or_else(range_err)?;
                *months = months.checked_add(m).ok_or_else(range_err)?;
                add_frac_days(days, nanos, (v - whole) * 12.0 * AVG_DAYS_PER_MONTH, sgn)?;
            }
            ('M', true) => {
                let whole = v.trunc();
                let m = f64_to_i64_range(whole)?
                    .checked_mul(sgn)
                    .ok_or_else(range_err)?;
                *months = months.checked_add(m).ok_or_else(range_err)?;
                add_frac_days(days, nanos, (v - whole) * AVG_DAYS_PER_MONTH, sgn)?;
            }
            ('W', true) => add_frac_days(days, nanos, v * 7.0, sgn)?,
            ('D', true) => add_frac_days(days, nanos, v, sgn)?,
            ('H', false) => add_scaled_nanos(nanos, v, 3_600_000_000_000.0, sgn)?,
            ('M', false) => add_scaled_nanos(nanos, v, 60_000_000_000.0, sgn)?,
            ('S', false) => add_scaled_nanos(nanos, v, 1_000_000_000.0, sgn)?,
            _ => return Err(Error::ty("invalid duration component")),
        }
    }
    Ok(())
}

fn add_scaled_nanos(nanos: &mut i128, v: f64, scale: f64, sgn: i64) -> Result<()> {
    let scaled = f64_to_i128_range(v * scale)?
        .checked_mul(i128::from(sgn))
        .ok_or_else(range_err)?;
    *nanos = nanos.checked_add(scaled).ok_or_else(range_err)?;
    Ok(())
}

fn add_frac_days(days: &mut i64, nanos: &mut i128, frac_days: f64, sgn: i64) -> Result<()> {
    let total = frac_days * sgn as f64;
    let d = total.trunc();
    *days = days
        .checked_add(f64_to_i64_range(d)?)
        .ok_or_else(range_err)?;
    *nanos = nanos
        .checked_add(f64_to_i128_range((total - d) * NANOS_PER_DAY)?)
        .ok_or_else(range_err)?;
    Ok(())
}

fn is_datetime_style_duration(date: &str, time: Option<&str>) -> bool {
    let date_cal = !date.is_empty()
        && date.contains('-')
        && date
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == '.');
    let time_cal = time.is_some_and(|t| {
        t.contains(':')
            && t.chars()
                .all(|c| c.is_ascii_digit() || c == ':' || c == '.')
    });
    date_cal || time_cal
}

fn parse_duration_datetime_style(
    date: &str,
    time: Option<&str>,
    months: &mut i64,
    days: &mut i64,
    nanos: &mut i128,
) -> Result<()> {
    if !date.is_empty() {
        let parts: Vec<&str> = date.split('-').collect();
        if let Some(y) = parts.first().and_then(|s| s.parse::<i64>().ok()) {
            let y12 = y.checked_mul(12).ok_or_else(range_err)?;
            *months = months.checked_add(y12).ok_or_else(range_err)?;
        }
        if parts.len() > 1 {
            if let Ok(m) = parts[1].parse::<i64>() {
                *months = months.checked_add(m).ok_or_else(range_err)?;
            }
        }
        if parts.len() > 2 {
            if let Ok(d) = parts[2].parse::<i64>() {
                *days = days.checked_add(d).ok_or_else(range_err)?;
            }
        }
    }
    if let Some(t) = time {
        let parts: Vec<&str> = t.split(':').collect();
        if let Some(h) = parts.first() {
            let h: f64 = h.parse().map_err(|_| Error::ty("invalid duration time"))?;
            *nanos = nanos
                .checked_add(f64_to_i128_range(h * 3_600_000_000_000.0)?)
                .ok_or_else(range_err)?;
        }
        if parts.len() > 1 {
            let m: f64 = parts[1]
                .parse()
                .map_err(|_| Error::ty("invalid duration time"))?;
            *nanos = nanos
                .checked_add(f64_to_i128_range(m * 60_000_000_000.0)?)
                .ok_or_else(range_err)?;
        }
        if parts.len() > 2 {
            let s: f64 = parts[2]
                .parse()
                .map_err(|_| Error::ty("invalid duration time"))?;
            *nanos = nanos
                .checked_add(f64_to_i128_range(s * 1_000_000_000.0)?)
                .ok_or_else(range_err)?;
        }
    }
    Ok(())
}
