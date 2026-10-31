//! Zone resolution: classifying a zone string, resolving an IANA zone to
//! its UTC offset at a wall clock or at a UTC instant, and the
//! [`ResolvedZone`] a `timezone` field names (ADR-0178; extracted from
//! `construct.rs` per ADR-0340).

use chrono::{NaiveDateTime, Offset, TimeZone};
use chrono_tz::Tz;

use crate::{Error, Result, Value};

use super::parse::parse_offset;
use super::{map_temporal, Temporal};

/// How a zone string is spelled. Classified once so every consumer matches
/// on the kind instead of re-probing the string's shape: the formatting side
/// in `mod.rs` and the resolution side here cannot drift apart.
pub(super) enum ZoneSpec<'a> {
    /// An IANA name (`Europe/Stockholm`).
    Iana(&'a str),
    /// `Z` / `z`: UTC.
    Utc,
    /// A numeric offset spelling (`+01:00`, `-0500`, …).
    Fixed(&'a str),
    /// Any other string. The map path still attempts it as an offset
    /// (`parse_offset` accepts e.g. `UTC`); the formatting path falls back
    /// to the stored numeric offset for it.
    Other(&'a str),
}

pub(super) fn zone_kind(z: &str) -> ZoneSpec<'_> {
    if z.contains('/') {
        ZoneSpec::Iana(z)
    } else if z.eq_ignore_ascii_case("Z") {
        ZoneSpec::Utc
    } else if z.starts_with('+') || z.starts_with('-') {
        ZoneSpec::Fixed(z)
    } else {
        ZoneSpec::Other(z)
    }
}

/// Why an IANA wall clock could not be resolved to a UTC offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneError {
    /// `zone` does not name a zone in the engine's IANA database.
    UnknownZone,
    /// `local` falls in a DST gap: the wall clock never occurs in `zone`.
    NonexistentWallTime,
}

/// Resolve the IANA wall clock `local` in `zone` to its UTC offset in
/// seconds (ADR-0178).
///
/// A unique wall time yields its offset; a DST overlap yields the earliest
/// (pre-transition) occurrence's offset. An unknown `zone` name and a wall
/// time inside a DST gap are the two error outcomes.
pub fn zone_offset_at(zone: &str, local: NaiveDateTime) -> std::result::Result<i32, ZoneError> {
    let tz: Tz = zone.parse().map_err(|_| ZoneError::UnknownZone)?;
    let dt = tz
        .from_local_datetime(&local)
        .single()
        .or_else(|| tz.from_local_datetime(&local).earliest())
        .ok_or(ZoneError::NonexistentWallTime)?;
    Ok(dt.offset().fix().local_minus_utc())
}

/// Resolve the IANA zone `zone` at the UTC instant `utc` to its UTC offset
/// in seconds (ADR-0329, ADR-0340). Unlike a wall clock
/// ([`zone_offset_at`]), an instant is never ambiguous, so an unknown zone
/// is the only failure (`ZoneError::NonexistentWallTime` is unreachable
/// here). The Bolt DateTimeZoneIdUTC decoder and UTC-instant zone
/// conversion share this rule.
pub fn zone_offset_at_utc(zone: &str, utc: NaiveDateTime) -> std::result::Result<i32, ZoneError> {
    let tz: Tz = zone.parse().map_err(|_| ZoneError::UnknownZone)?;
    Ok(tz.from_utc_datetime(&utc).offset().fix().local_minus_utc())
}

/// The IANA region a `Temporal::DateTime` zone names, or `None` for an
/// offset-only zone (no zone, `Z`, a numeric offset, any other spelling).
/// The typed classification behind the Bolt DateTime vs DateTimeZoneId
/// struct choice (ADR-0325), on the same [`zone_kind`] rule the formatter
/// uses.
pub fn zone_region(zone: Option<&str>) -> Option<&str> {
    match zone.map(zone_kind) {
        Some(ZoneSpec::Iana(name)) => Some(name),
        Some(ZoneSpec::Utc | ZoneSpec::Fixed(_) | ZoneSpec::Other(_)) | None => None,
    }
}

/// The reading an IANA zone offset is resolved at: a local wall-clock time
/// (constructing a zoned value from local fields) or a UTC instant
/// (converting an already-UTC value into the zone). The one discriminator
/// [`offset_from_map`] takes, so the map probe exists once.
pub(super) enum IanaAt {
    Local(NaiveDateTime),
    Utc(NaiveDateTime),
}

/// One failure vocabulary for both readings: the resolver's typed outcome
/// mapped back to this path's message (ADR-0178, ADR-0340).
fn iana_offset_at(zone: &str, at: IanaAt) -> Result<i32> {
    let resolved = match at {
        IanaAt::Local(local) => zone_offset_at(zone, local),
        IanaAt::Utc(utc) => zone_offset_at_utc(zone, utc),
    };
    resolved.map_err(|e| match e {
        ZoneError::UnknownZone => Error::ty(format!("unknown timezone '{zone}'")),
        ZoneError::NonexistentWallTime => Error::ty("ambiguous datetime"),
    })
}

/// The offset and zone a `timezone` field (or an inherited source) names:
/// the domain concept the constructors carry between signatures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResolvedZone {
    /// UTC offset in seconds.
    pub(super) offset: i32,
    /// The zone spelling to keep, or `None` for an offset-only source.
    pub(super) zone: Option<String>,
}

impl ResolvedZone {
    /// UTC with zone `Z`: the documented default when no zone is named.
    pub(super) fn utc() -> Self {
        Self {
            offset: 0,
            zone: Some("Z".into()),
        }
    }
}

/// The [`ResolvedZone`] a `timezone` string names, resolved at `at`: the
/// single `zone_kind` match behind [`offset_from_map`].
fn offset_for_zone(z: &str, at: IanaAt) -> Result<ResolvedZone> {
    Ok(match zone_kind(z) {
        ZoneSpec::Iana(name) => ResolvedZone {
            offset: iana_offset_at(name, at)?,
            zone: Some(String::from(z)),
        },
        ZoneSpec::Utc => ResolvedZone::utc(),
        ZoneSpec::Fixed(s) | ZoneSpec::Other(s) => ResolvedZone {
            offset: parse_offset(s)?,
            zone: Some(String::from(z)),
        },
    })
}

/// The [`ResolvedZone`] a map's `timezone` field names, resolved at `at`
/// (ADR-0340): the one map entry point, for both the local wall-clock and
/// the UTC-instant readings ([`IanaAt`]). A caller with no instant of its
/// own states the `now()` it resolves the zone at. A missing field is the
/// documented UTC default; a present but non-string value is a typed error,
/// never a silent default.
pub(super) fn offset_from_map(
    m: &std::collections::BTreeMap<String, Value>,
    at: IanaAt,
) -> Result<ResolvedZone> {
    match m.get("timezone") {
        None => Ok(ResolvedZone::utc()),
        Some(Value::String(z)) => offset_for_zone(z, at),
        Some(_) => Err(Error::invalid_argument_type("timezone must be STRING")),
    }
}

/// The [`ResolvedZone`] a map's inherited source contributes, probing the
/// `time` and `datetime` fields in that order: `Time` contributes its
/// offset only, `DateTime` contributes its offset plus zone, and any other
/// value yields `None` (no inheritable source). [`apply_timezone`] resolves
/// the zone at the local wall clock when the map names its own timezone.
fn source_offset_zone(m: &std::collections::BTreeMap<String, Value>) -> Option<ResolvedZone> {
    for k in ["time", "datetime"] {
        if let Some(t) = map_temporal(m, k) {
            match t {
                Temporal::Time { offset, .. } => {
                    return Some(ResolvedZone {
                        offset: *offset,
                        zone: None,
                    })
                }
                Temporal::DateTime { offset, zone, .. } => {
                    return Some(ResolvedZone {
                        offset: *offset,
                        zone: zone.clone(),
                    })
                }
                _ => {}
            }
        }
    }
    None
}

/// `utc + offset seconds` is the zone's wall clock at the UTC instant `utc`,
/// erroring instead of panicking when the result would fall outside chrono's
/// range. The additive twin of [`sub_offset`].
pub(super) fn add_offset(utc: NaiveDateTime, offset: i32) -> Result<NaiveDateTime> {
    utc.checked_add_signed(chrono::Duration::seconds(i64::from(offset)))
        .ok_or_else(super::range_err)
}

/// `local - offset seconds`, erroring instead of panicking when the result
/// would fall outside chrono's range.
pub(super) fn sub_offset(local: NaiveDateTime, offset: i32) -> Result<NaiveDateTime> {
    local
        .checked_sub_signed(chrono::Duration::seconds(i64::from(offset)))
        .ok_or_else(super::range_err)
}

/// `(utc, zone)` after inheriting or converting a map timezone. The
/// inherited source is derived from the same map ([`source_offset_zone`]),
/// so callers never thread it.
pub(super) fn apply_timezone(
    local: NaiveDateTime,
    m: &std::collections::BTreeMap<String, Value>,
) -> Result<(NaiveDateTime, ResolvedZone)> {
    let source = source_offset_zone(m);
    if m.contains_key("timezone") {
        if let Some(src) = source {
            // Resolve the source zone at the local wall clock, otherwise
            // keep the stored offset.
            let src_off = source_offset(&src, local)?;
            let utc = sub_offset(local, src_off)?;
            return Ok((utc, offset_from_map(m, IanaAt::Utc(utc))?));
        }
        let resolved = offset_from_map(m, IanaAt::Local(local))?;
        let utc = sub_offset(local, resolved.offset)?;
        return Ok((utc, resolved));
    }
    if let Some(src) = source {
        let offset = source_offset(&src, local)?;
        let utc = sub_offset(local, offset)?;
        return Ok((
            utc,
            ResolvedZone {
                offset,
                zone: src.zone,
            },
        ));
    }
    Ok((local, ResolvedZone::utc()))
}

/// Resolve an inherited [`ResolvedZone`] at the local wall clock: an IANA
/// zone's offset for `local`, anything else keeps the stored offset.
fn source_offset(source: &ResolvedZone, local: NaiveDateTime) -> Result<i32> {
    match source.zone.as_deref().map(zone_kind) {
        Some(ZoneSpec::Iana(name)) => iana_offset_at(name, IanaAt::Local(local)),
        _ => Ok(source.offset),
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::*;

    /// ADR-0340: the UTC-instant resolver shares the typed failure
    /// vocabulary; an instant is never ambiguous, so an unknown zone is
    /// the only error.
    #[test]
    fn zone_offset_at_utc_is_typed_and_unambiguous() {
        let at =
            |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M").expect("test instant");
        // Either side of the 2021-10-31 Stockholm fall-back: two instants,
        // two offsets.
        assert_eq!(
            zone_offset_at_utc("Europe/Stockholm", at("2021-10-31T00:30")),
            Ok(7200)
        );
        assert_eq!(
            zone_offset_at_utc("Europe/Stockholm", at("2021-10-31T01:30")),
            Ok(3600)
        );
        assert_eq!(
            zone_offset_at_utc("Mars/Base", at("2021-10-31T01:30")),
            Err(ZoneError::UnknownZone)
        );
        // The wall-clock resolver's gap outcome cannot arise at an instant:
        // the local reading 2021-03-28T02:30 does not exist, while the UTC
        // instant 00:30 resolves normally (01:30 CET, before the switch).
        let gap = NaiveDate::from_ymd_opt(2021, 3, 28)
            .unwrap()
            .and_hms_opt(2, 30, 0)
            .unwrap();
        assert_eq!(
            zone_offset_at("Europe/Stockholm", gap),
            Err(ZoneError::NonexistentWallTime)
        );
        let before_transition = NaiveDate::from_ymd_opt(2021, 3, 28)
            .unwrap()
            .and_hms_opt(0, 30, 0)
            .unwrap();
        assert_eq!(
            zone_offset_at_utc("Europe/Stockholm", before_transition),
            Ok(3600)
        );
    }

    /// F-1-27: a missing `timezone` key is the documented UTC default; a
    /// present but non-string value is a typed error, never a silent default.
    #[test]
    fn offset_from_map_distinguishes_missing_from_malformed_timezone() {
        let at = || {
            IanaAt::Local(
                NaiveDate::from_ymd_opt(2021, 1, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            )
        };
        let missing = std::collections::BTreeMap::new();
        assert_eq!(
            offset_from_map(&missing, at()),
            Ok(ResolvedZone {
                offset: 0,
                zone: Some("Z".into()),
            })
        );
        let malformed = [("timezone".into(), Value::Int(42))]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            offset_from_map(&malformed, at()),
            Err(Error::invalid_argument_type("timezone must be STRING"))
        );
    }

    /// `iana_offset_at` renders the same `unknown timezone` message for
    /// both readings (the message single-sourced with ADR-0340).
    #[test]
    fn iana_offset_at_renders_one_unknown_zone_message() {
        let local = NaiveDate::from_ymd_opt(2021, 3, 28)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let expected = Error::ty("unknown timezone 'Not/AZone'");
        assert_eq!(
            iana_offset_at("Not/AZone", IanaAt::Local(local)),
            Err(expected.clone())
        );
        assert_eq!(
            iana_offset_at("Not/AZone", IanaAt::Utc(local)),
            Err(expected)
        );
    }
}
