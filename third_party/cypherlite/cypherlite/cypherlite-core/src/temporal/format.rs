//! CIP-223 temporal pattern formatting.

use chrono::{Datelike, Timelike, Weekday};

use crate::{Error, Result};

use super::zone::add_offset;
use super::{format_offset, time_to_nanos, Temporal};

/// Storage-independent subset of the CIP-223/Java temporal pattern surface.
/// Unsupported alphabetic pattern symbols are rejected instead of emitted
/// literally, so callers get a deterministic Cypher type error.
pub fn format_pattern(value: &Temporal, pattern: &str) -> Result<String> {
    if pattern.chars().any(|c| matches!(c, '#' | '{' | '}')) {
        return Err(Error::syntax("InvalidPatternCharacter"));
    }
    let (date, time, offset, zone) = match value {
        Temporal::Date(d) => (Some(*d), None, None, None),
        Temporal::LocalTime(t) => (None, Some(*t), None, None),
        Temporal::Time { time, offset } => (None, Some(*time), Some(*offset), None),
        Temporal::LocalDateTime(dt) => (Some(dt.date()), Some(dt.time()), None, None),
        Temporal::DateTime { utc, offset, zone } => {
            let local = add_offset(*utc, *offset)?;
            (
                Some(local.date()),
                Some(local.time()),
                Some(*offset),
                zone.as_deref(),
            )
        }
        Temporal::Duration {
            months,
            days,
            nanos,
        } => return format_duration_pattern(*months, *days, *nanos, pattern),
        Temporal::CivilDate { .. } | Temporal::CivilDateTime { .. } => {
            return if pattern.is_empty() {
                Ok(String::new())
            } else {
                Err(Error::ty(
                    "format pattern is unsupported for out-of-range dates",
                ))
            }
        }
    };
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let mut quoted = false;
    while i < chars.len() {
        if chars[i] == '\'' {
            if chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
                continue;
            }
            quoted = !quoted;
            i += 1;
            continue;
        }
        if !quoted && matches!(chars[i], '[' | ']') {
            i += 1;
            continue;
        }
        if quoted || !chars[i].is_ascii_alphabetic() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let c = chars[i];
        let mut len = 1;
        while chars.get(i + len) == Some(&c) {
            len += 1;
        }
        if c == 'p' {
            let Some(&next) = chars.get(i + len) else {
                return Err(Error::ty("InvalidPatternCharacter"));
            };
            let mut next_len = 1;
            while chars.get(i + len + next_len) == Some(&next) {
                next_len += 1;
            }
            let rendered = format_pattern(value, &next.to_string().repeat(next_len))?;
            out.push_str(&format!("{rendered:>len$}"));
            i += len + next_len;
            continue;
        }
        let piece = match c {
            'y' | 'u' | 'Y' => {
                let d = date.ok_or_else(|| Error::ty("date field unavailable"))?;
                let y = if c == 'Y' {
                    d.iso_week().year()
                } else if c == 'y' && d.year() <= 0 {
                    1 - d.year()
                } else {
                    d.year()
                };
                if len == 2 {
                    format!("{:02}", y.rem_euclid(100))
                } else if y < 0 {
                    format!("-{:0len$}", y.unsigned_abs())
                } else {
                    format!("{y:0len$}")
                }
            }
            'M' | 'L' => {
                let month = date
                    .ok_or_else(|| Error::ty("date field unavailable"))?
                    .month();
                if len > 5 {
                    return Err(Error::syntax("InvalidPatternCharacter"));
                } else if len >= 3 {
                    let full = [
                        "January",
                        "February",
                        "March",
                        "April",
                        "May",
                        "June",
                        "July",
                        "August",
                        "September",
                        "October",
                        "November",
                        "December",
                    ][month as usize - 1];
                    match len {
                        3 => full.chars().take(3).collect(),
                        4 => full.to_string(),
                        5 => full.chars().next().unwrap().to_string(),
                        _ => return Err(Error::syntax("InvalidPatternCharacter")),
                    }
                } else {
                    format!("{month:0len$}")
                }
            }
            'q' | 'Q' => {
                let q = (date
                    .ok_or_else(|| Error::ty("date field unavailable"))?
                    .month0()
                    / 3)
                    + 1;
                match len {
                    1 => q.to_string(),
                    2 => format!("{q:02}"),
                    3 => format!("Q{q}"),
                    4 => match q {
                        1 => "1st quarter".into(),
                        2 => "2nd quarter".into(),
                        3 => "3rd quarter".into(),
                        _ => "4th quarter".into(),
                    },
                    5 => q.to_string(),
                    _ => return Err(Error::syntax("InvalidPatternCharacter")),
                }
            }
            'd' => format!(
                "{:0len$}",
                if len > 2 {
                    return Err(Error::syntax("InvalidPatternCharacter"));
                } else {
                    date.ok_or_else(|| Error::ty("date field unavailable"))?
                        .day()
                }
            ),
            'D' => format!(
                "{:0len$}",
                date.ok_or_else(|| Error::ty("date field unavailable"))?
                    .ordinal()
            ),
            'E' => {
                let weekday = date
                    .ok_or_else(|| Error::ty("date field unavailable"))?
                    .weekday();
                let name = match weekday {
                    Weekday::Mon => "Monday",
                    Weekday::Tue => "Tuesday",
                    Weekday::Wed => "Wednesday",
                    Weekday::Thu => "Thursday",
                    Weekday::Fri => "Friday",
                    Weekday::Sat => "Saturday",
                    Weekday::Sun => "Sunday",
                };
                if len >= 4 {
                    name.into()
                } else {
                    name.chars().take(3).collect()
                }
            }
            'e' | 'c' => (date
                .ok_or_else(|| Error::ty("date field unavailable"))?
                .weekday()
                .num_days_from_sunday()
                + 1)
            .to_string(),
            'w' => date
                .ok_or_else(|| Error::ty("date field unavailable"))?
                .iso_week()
                .week()
                .to_string(),
            'W' => {
                let date = date.ok_or_else(|| Error::ty("date field unavailable"))?;
                let first = date.with_day(1).unwrap().weekday().num_days_from_sunday();
                ((date.day() + first - 1) / 7 + 1).to_string()
            }
            'F' => (date
                .ok_or_else(|| Error::ty("date field unavailable"))?
                .day0()
                / 7
                + 1)
            .to_string(),
            'g' => (date
                .ok_or_else(|| Error::ty("date field unavailable"))?
                .num_days_from_ce()
                - 678_576)
                .to_string(),
            'G' => {
                if date
                    .ok_or_else(|| Error::ty("date field unavailable"))?
                    .year()
                    <= 0
                {
                    "BC".into()
                } else {
                    "AD".into()
                }
            }
            'H' => format!(
                "{:0len$}",
                time.ok_or_else(|| Error::ty("time field unavailable"))?
                    .hour()
            ),
            'k' => {
                let h = time
                    .ok_or_else(|| Error::ty("time field unavailable"))?
                    .hour();
                format!("{:0len$}", if h == 0 { 24 } else { h })
            }
            'h' => {
                let h = time
                    .ok_or_else(|| Error::ty("time field unavailable"))?
                    .hour()
                    % 12;
                format!("{:0len$}", if h == 0 { 12 } else { h })
            }
            'K' => format!(
                "{:0len$}",
                time.ok_or_else(|| Error::ty("time field unavailable"))?
                    .hour()
                    % 12
            ),
            'a' => {
                if time
                    .ok_or_else(|| Error::ty("time field unavailable"))?
                    .hour()
                    < 12
                {
                    "AM".into()
                } else {
                    "PM".into()
                }
            }
            'B' => {
                let hour = time
                    .ok_or_else(|| Error::ty("time field unavailable"))?
                    .hour();
                match hour {
                    0..=5 => "at night",
                    6..=11 => "in the morning",
                    12..=17 => "in the afternoon",
                    _ => "in the evening",
                }
                .into()
            }
            'm' => format!(
                "{:0len$}",
                time.ok_or_else(|| Error::ty("time field unavailable"))?
                    .minute()
            ),
            's' => format!(
                "{:0len$}",
                time.ok_or_else(|| Error::ty("time field unavailable"))?
                    .second()
            ),
            'S' => {
                let nanos = time
                    .ok_or_else(|| Error::ty("time field unavailable"))?
                    .nanosecond();
                format!("{nanos:09}").chars().take(len.min(9)).collect()
            }
            'n' => time
                .ok_or_else(|| Error::ty("time field unavailable"))?
                .nanosecond()
                .to_string(),
            'N' => {
                time_to_nanos(time.ok_or_else(|| Error::ty("time field unavailable"))?).to_string()
            }
            'A' => (time_to_nanos(time.ok_or_else(|| Error::ty("time field unavailable"))?)
                / 1_000_000)
                .to_string(),
            'Z' => {
                let offset = offset.ok_or_else(|| Error::ty("offset field unavailable"))?;
                match len {
                    1..=3 => format_offset(offset).replace(':', ""),
                    4 if offset == 0 => "GMT".into(),
                    4 => format!("GMT{}", format_offset(offset)),
                    5 if offset == 0 => "Z".into(),
                    5 => format_offset(offset),
                    _ => return Err(Error::syntax("InvalidPatternCharacter")),
                }
            }
            'X' | 'x' => {
                let offset = offset.ok_or_else(|| Error::ty("offset field unavailable"))?;
                if offset == 0 && c == 'X' {
                    "Z".into()
                } else {
                    let raw = format_offset(offset);
                    let compact = raw.replace(':', "");
                    match len {
                        1 => {
                            if offset % 3600 == 0 {
                                compact[..3].into()
                            } else {
                                compact
                            }
                        }
                        2 | 4 => compact,
                        3 | 5 => raw,
                        _ => return Err(Error::syntax("InvalidPatternCharacter")),
                    }
                }
            }
            'O' => {
                let offset = offset.ok_or_else(|| Error::ty("offset field unavailable"))?;
                match (len, offset) {
                    (1 | 4, 0) => "GMT".into(),
                    (1, _) if offset % 3600 == 0 => {
                        format!("GMT{:+}", offset / 3600)
                    }
                    (1, _) => format!("GMT{}", format_offset(offset)),
                    (4, _) => format!("GMT{}", format_offset(offset)),
                    _ => return Err(Error::syntax("InvalidPatternCharacter")),
                }
            }
            'z' | 'v' => {
                let zone = zone.ok_or_else(|| Error::syntax("InvalidPatternCharacter"))?;
                if (c == 'z' && len > 4) || (c == 'v' && !matches!(len, 1 | 4)) {
                    return Err(Error::syntax("InvalidPatternCharacter"));
                }
                match (zone, c, len) {
                    ("Europe/Paris", 'z', 4) => "Central European Summer Time".into(),
                    ("Europe/Paris", 'v', 4) => "Central European Time".into(),
                    ("Europe/Paris", 'z', _) => "CEST".into(),
                    ("Europe/Paris", 'v', _) => "CET".into(),
                    _ => zone.into(),
                }
            }
            'V' if len == 2 && date.is_some() => zone
                .map(str::to_string)
                .unwrap_or_else(|| format_offset(offset.unwrap_or(0))),
            _ => {
                return Err(Error::ty(format!(
                    "unsupported temporal format symbol '{c}'"
                )))
            }
        };
        out.push_str(&piece);
        i += len;
    }
    if quoted {
        return Err(Error::ty("unterminated quote in temporal format"));
    }
    Ok(out)
}

fn format_duration_pattern(months: i64, days: i64, nanos: i128, pattern: &str) -> Result<String> {
    const FIELDS: &str = "yYuqQMLwWdDhHkKmsnSAN";
    #[derive(Clone)]
    enum Token {
        Literal(String),
        Field(char, usize),
    }
    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    enum Level {
        Years,
        Quarters,
        Months,
        Weeks,
        Days,
        Hours,
        Minutes,
        Seconds,
        Millis,
        Nanos,
    }
    fn level(c: char) -> Option<Level> {
        Some(match c {
            'y' | 'Y' | 'u' => Level::Years,
            'q' | 'Q' => Level::Quarters,
            'M' | 'L' => Level::Months,
            'w' | 'W' => Level::Weeks,
            'd' | 'D' => Level::Days,
            'h' | 'H' | 'k' | 'K' => Level::Hours,
            'm' => Level::Minutes,
            's' => Level::Seconds,
            'A' => Level::Millis,
            'N' => Level::Nanos,
            _ => return None,
        })
    }
    fn cascade(
        total: i128,
        levels: &[(Level, i128)],
        present: &std::collections::BTreeSet<Level>,
    ) -> std::collections::BTreeMap<Level, i128> {
        let mut remainder = total;
        let mut values = std::collections::BTreeMap::new();
        for (level, unit) in levels {
            if present.contains(level) {
                values.insert(*level, remainder / unit);
                remainder %= unit;
            }
        }
        values
    }
    fn component(value: i128, width: usize) -> String {
        let magnitude = value.unsigned_abs();
        let digits = if width <= 1 {
            magnitude.to_string()
        } else {
            format!("{magnitude:0width$}")
        };
        if value < 0 {
            format!("-{digits}")
        } else {
            digits
        }
    }

    let chars: Vec<char> = pattern.chars().collect();
    let mut tokens = Vec::new();
    let mut literal = String::new();
    let mut i = 0;
    let mut quoted = false;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            if i + 1 < chars.len() && chars[i + 1] == '\'' {
                literal.push('\'');
                i += 2;
                continue;
            }
            quoted = !quoted;
            i += 1;
            continue;
        }
        if !quoted && FIELDS.contains(c) {
            if !literal.is_empty() {
                tokens.push(Token::Literal(std::mem::take(&mut literal)));
            }
            let mut count = 1;
            while i + count < chars.len() && chars[i + count] == c {
                count += 1;
            }
            tokens.push(Token::Field(c, count));
            i += count;
        } else {
            literal.push(c);
            i += 1;
        }
    }
    if quoted {
        return Err(Error::ty("unterminated quote in duration pattern"));
    }
    if !literal.is_empty() {
        tokens.push(Token::Literal(literal));
    }
    let present: std::collections::BTreeSet<Level> = tokens
        .iter()
        .filter_map(|t| match t {
            Token::Field(c, _) => level(*c),
            Token::Literal(_) => None,
        })
        .collect();
    let month_values = cascade(
        months as i128,
        &[(Level::Years, 12), (Level::Quarters, 3), (Level::Months, 1)],
        &present,
    );
    let day_values = cascade(
        days as i128,
        &[(Level::Weeks, 7), (Level::Days, 1)],
        &present,
    );
    let second_values = cascade(
        nanos,
        &[
            (Level::Hours, 3_600_000_000_000),
            (Level::Minutes, 60_000_000_000),
            (Level::Seconds, 1_000_000_000),
            (Level::Millis, 1_000_000),
            (Level::Nanos, 1),
        ],
        &present,
    );
    let fraction = format!("{:09}", nanos.unsigned_abs() % 1_000_000_000);
    let mut out = String::new();
    for token in tokens {
        match token {
            Token::Literal(s) => out.push_str(&s),
            Token::Field('n' | 'S', count) => {
                if !(1..=9).contains(&count) {
                    return Err(Error::ty("InvalidPatternCharacter"));
                }
                out.push_str(&fraction[..count]);
            }
            Token::Field(c, count) => {
                let key = level(c).unwrap();
                let value = month_values
                    .get(&key)
                    .or_else(|| day_values.get(&key))
                    .or_else(|| second_values.get(&key))
                    .copied()
                    .unwrap_or(0);
                out.push_str(&component(value, count));
            }
        }
    }
    Ok(out)
}
