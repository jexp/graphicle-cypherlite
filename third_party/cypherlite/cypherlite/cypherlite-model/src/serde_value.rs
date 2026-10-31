//! Serialization/deserialization support for CypherValue.
//!
//! Converts CypherValue to/from serde_json::Value for JSON persistence.
//! Designed for Go JSON format compatibility.

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
use indexmap::IndexMap;
use serde_json::{Map as JsonMap, Value as JsonValue};

use crate::value::{Coord2D, CypherValue, GeometryValue, IsoDuration, PointValue, VectorCoordKind};

/// Invalid persistence value, with a JSON path to the offending member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for EncodeError {}

fn encode_error(path: &str, message: impl Into<String>) -> EncodeError {
    EncodeError {
        path: path.into(),
        message: message.into(),
    }
}

/// A finite f64 is exactly representable as f32 only if its signed bits survive.
pub fn float32_exact(f: f64) -> bool {
    f.is_finite() && ((f as f32) as f64).to_bits() == f.to_bits()
}

fn finite_number(f: f64, path: &str) -> Result<JsonValue, EncodeError> {
    serde_json::Number::from_f64(f)
        .map(JsonValue::Number)
        .ok_or_else(|| encode_error(path, "expected finite number"))
}

fn require_minute_offset(offset: &FixedOffset, path: &str) -> Result<(), EncodeError> {
    if offset.local_minus_utc() % 60 != 0 {
        return Err(encode_error(
            path,
            "offset must be a whole number of minutes",
        ));
    }
    Ok(())
}

/// Encode a persistence value as v2 JSON, rejecting values that cannot round-trip.
/// Temporal, spatial, duration and vector values use `__type` tags, while user
/// maps containing `__type` use the versioned `map` escape.
/// A `DateTime` also requires a minute-aligned fixed offset whose UTC instant
/// plus offset fits chrono's local `NaiveDateTime` range. Otherwise this returns
/// an [`EncodeError`] with the nested value path before RFC3339 formatting;
/// `serde::Serialize` reports the same error.
///
/// ```
/// use cypherlite_model::{CypherValue, serde_value::{
///     cypher_value_to_json, props_to_json, try_json_to_cypher_value_v2,
/// }};
/// use cypherlite_model::value::PointValue;
/// use indexmap::IndexMap;
///
/// let point = CypherValue::Point(PointValue::cartesian_3d(1.5, -2.25, 3.0));
/// let value = CypherValue::Map(IndexMap::from([
///     ("__type".into(), CypherValue::Str("user data".into())),
///     ("place".into(), point),
/// ]));
/// let json = cypher_value_to_json(&value)?;
/// assert_eq!(json["__type"], "map");
/// assert_eq!(json["value"]["place"]["__type"], "point");
/// assert_eq!(try_json_to_cypher_value_v2(&json)?, value);
///
/// let invalid = CypherValue::Point(PointValue::cartesian_3d(1.5, f64::INFINITY, 3.0));
/// assert_eq!(cypher_value_to_json(&invalid).unwrap_err().path, "$.y");
/// let props = IndexMap::from([("place".into(), invalid)]);
/// assert_eq!(props_to_json(&props).unwrap_err().path, "$.place.y");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// A fixed offset can be valid even when its local time lies outside chrono's
/// range. Canonical expanded-year values *inside* the range still round-trip:
///
/// ```
/// use chrono::{Duration, FixedOffset, NaiveDateTime, TimeZone};
/// use cypherlite_model::{CypherValue, serde_value::{
///     cypher_value_to_json, props_to_json, try_json_to_cypher_value_v2,
/// }};
/// use indexmap::IndexMap;
///
/// let plus_one = FixedOffset::east_opt(3600).unwrap();
/// let invalid = CypherValue::DateTime(plus_one.from_utc_datetime(&NaiveDateTime::MAX), None);
/// let error = cypher_value_to_json(&invalid).unwrap_err();
/// assert_eq!(error.path, "$");
/// assert_eq!(error.message, "DateTime local time out of range");
/// let nested = CypherValue::Map(IndexMap::from([("items".into(), CypherValue::List(vec![invalid]))]));
/// let props = IndexMap::from([("payload".into(), nested)]);
/// assert_eq!(props_to_json(&props).unwrap_err().path, "$.payload.items[0]");
/// assert!(serde_json::to_value(&CypherValue::Map(props)).unwrap_err()
///     .to_string().contains("$.payload.items[0]"));
///
/// for (utc, offset, expected) in [
///     (NaiveDateTime::MAX - Duration::hours(1), 3600,
///      "+262142-12-31T23:59:59.999999999+01:00"),
///     (NaiveDateTime::MIN + Duration::hours(1), -3600,
///      "-262143-01-01T00:00:00-01:00"),
/// ] {
///     let fixed = FixedOffset::east_opt(offset).unwrap();
///     let value = CypherValue::DateTime(fixed.from_utc_datetime(&utc), None);
///     let json = cypher_value_to_json(&value)?;
///     assert_eq!(json["value"], expected);
///     assert_eq!(try_json_to_cypher_value_v2(&json)?, value);
///     assert_eq!(serde_json::from_value::<CypherValue>(serde_json::to_value(&value)?)?, value);
/// }
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn cypher_value_to_json(val: &CypherValue) -> Result<JsonValue, EncodeError> {
    encode(val, "$")
}

fn encode(val: &CypherValue, path: &str) -> Result<JsonValue, EncodeError> {
    Ok(match val {
        CypherValue::Null => JsonValue::Null,
        CypherValue::Bool(b) => JsonValue::Bool(*b),
        CypherValue::Int(i) => JsonValue::Number((*i).into()),
        CypherValue::Float(f) => finite_number(*f, path)?,
        CypherValue::Str(s) => JsonValue::String(s.clone()),
        CypherValue::List(items) => JsonValue::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, item)| encode(item, &format!("{path}[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        CypherValue::Map(map) => {
            let obj = encode_map(map, path)?;
            if map.contains_key("__type") {
                serde_json::json!({"__type": "map", "value": obj})
            } else {
                JsonValue::Object(obj)
            }
        }
        CypherValue::Date(d) => serde_json::json!({"__type": "date", "value": d.to_string()}),
        CypherValue::Time(t, tz) => {
            let mut obj = serde_json::json!({
                "__type": "time", "value": t.format("%H:%M:%S%.f").to_string(),
            });
            if let Some(offset) = tz {
                require_minute_offset(offset, path)?;
                let secs = offset.local_minus_utc();
                let h = secs.abs() / 3600;
                let m = (secs.abs() % 3600) / 60;
                obj["offset"] =
                    JsonValue::String(format!("{}{h:02}:{m:02}", if secs < 0 { '-' } else { '+' }));
            }
            obj
        }
        CypherValue::DateTime(dt, tz_name) => {
            require_minute_offset(dt.offset(), path)?;
            if dt
                .naive_utc()
                .checked_add_signed(Duration::seconds(i64::from(dt.offset().local_minus_utc())))
                .is_none()
            {
                return Err(encode_error(path, "DateTime local time out of range"));
            }
            let mut obj = serde_json::json!({"__type": "datetime", "value": dt.to_rfc3339()});
            if let Some(name) = tz_name {
                obj["timezone"] = JsonValue::String(name.clone());
            }
            obj
        }
        CypherValue::LocalDateTime(ldt) => serde_json::json!({
            "__type": "localdatetime", "value": ldt.format("%Y-%m-%dT%H:%M:%S%.f").to_string(),
        }),
        CypherValue::Duration(d) => serde_json::json!({
            "__type": "duration", "months": d.months, "days": d.days,
            "seconds": d.seconds, "nanoseconds": d.nanoseconds,
        }),
        CypherValue::Point(p) => {
            let mut obj = serde_json::json!({"__type": "point", "srid": p.srid});
            obj["x"] = finite_number(p.x, &format!("{path}.x"))?;
            obj["y"] = finite_number(p.y, &format!("{path}.y"))?;
            if let Some(z) = p.z {
                obj["z"] = finite_number(z, &format!("{path}.z"))?;
            }
            obj
        }
        CypherValue::Geometry(g) => encode_geometry(g, path)?,
        CypherValue::Vector { coord, dim, values } => {
            if *dim != values.len() {
                return Err(encode_error(
                    &format!("{path}.dim"),
                    "dimension does not match values length",
                ));
            }
            let mut encoded = Vec::with_capacity(values.len());
            for (i, value) in values.iter().enumerate() {
                let at = format!("{path}.values[{i}]");
                let item = match (coord, value) {
                    (VectorCoordKind::Int8, CypherValue::Int(n)) if i8::try_from(*n).is_ok() => {
                        JsonValue::Number((*n).into())
                    }
                    (VectorCoordKind::Int16, CypherValue::Int(n)) if i16::try_from(*n).is_ok() => {
                        JsonValue::Number((*n).into())
                    }
                    (VectorCoordKind::Int32, CypherValue::Int(n)) if i32::try_from(*n).is_ok() => {
                        JsonValue::Number((*n).into())
                    }
                    (VectorCoordKind::Int64, CypherValue::Int(n)) => JsonValue::Number((*n).into()),
                    (VectorCoordKind::Float32, CypherValue::Float(f)) if float32_exact(*f) => {
                        finite_number(*f, &at)?
                    }
                    (VectorCoordKind::Float64, CypherValue::Float(f)) => finite_number(*f, &at)?,
                    _ => {
                        return Err(encode_error(
                            &at,
                            format!(
                                "invalid {} coordinate type or range",
                                coord.canonical_name()
                            ),
                        ))
                    }
                };
                encoded.push(item);
            }
            serde_json::json!({"__type": "vector", "coord": coord.canonical_name(), "dim": dim, "values": encoded})
        }
        CypherValue::Node { .. } | CypherValue::Relationship { .. } | CypherValue::Path { .. } => {
            return Err(encode_error(
                path,
                "graph entity is not a persistence property",
            ));
        }
    })
}

fn encode_map(
    map: &IndexMap<String, CypherValue>,
    path: &str,
) -> Result<JsonMap<String, JsonValue>, EncodeError> {
    map.iter()
        .map(|(key, value)| Ok((key.clone(), encode(value, &format!("{path}.{key}"))?)))
        .collect()
}

fn encode_coords(coords: &[Coord2D], path: &str) -> Result<JsonValue, EncodeError> {
    Ok(JsonValue::Array(
        coords
            .iter()
            .enumerate()
            .map(|(i, c)| {
                Ok(JsonValue::Array(vec![
                    finite_number(c[0], &format!("{path}[{i}][0]"))?,
                    finite_number(c[1], &format!("{path}[{i}][1]"))?,
                ]))
            })
            .collect::<Result<_, EncodeError>>()?,
    ))
}

fn encode_nested_coords(groups: &[Vec<Coord2D>], path: &str) -> Result<JsonValue, EncodeError> {
    Ok(JsonValue::Array(
        groups
            .iter()
            .enumerate()
            .map(|(i, coords)| encode_coords(coords, &format!("{path}[{i}]")))
            .collect::<Result<_, _>>()?,
    ))
}

fn encode_geometry(g: &GeometryValue, path: &str) -> Result<JsonValue, EncodeError> {
    let mut obj = serde_json::json!({
        "__type": "geometry", "geometry_type": g.type_name(), "srid": g.srid(),
    });
    let at = format!("{path}.coordinates");
    match g {
        GeometryValue::LineString { coordinates, .. } => {
            obj["coordinates"] = encode_coords(coordinates, &at)?
        }
        GeometryValue::Polygon { rings, .. } => {
            obj["coordinates"] = encode_nested_coords(rings, &at)?
        }
        GeometryValue::MultiPoint { points, .. } => {
            obj["coordinates"] = encode_coords(points, &at)?
        }
        GeometryValue::MultiLineString { lines, .. } => {
            obj["coordinates"] = encode_nested_coords(lines, &at)?
        }
        GeometryValue::MultiPolygon { polygons, .. } => {
            obj["coordinates"] = JsonValue::Array(
                polygons
                    .iter()
                    .enumerate()
                    .map(|(i, polygon)| encode_nested_coords(polygon, &format!("{at}[{i}]")))
                    .collect::<Result<_, _>>()?,
            );
        }
        GeometryValue::GeometryCollection { geometries, .. } => {
            obj["geometries"] = JsonValue::Array(
                geometries
                    .iter()
                    .enumerate()
                    .map(|(i, geometry)| {
                        encode_geometry(geometry, &format!("{path}.geometries[{i}]"))
                    })
                    .collect::<Result<_, _>>()?,
            );
        }
    }
    Ok(obj)
}

/// An invalid persistence value, with a JSON path to the offending field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

impl std::error::Error for DecodeError {}

fn invalid(path: &str, message: impl Into<String>) -> DecodeError {
    DecodeError {
        path: path.into(),
        message: message.into(),
    }
}

fn field<'a>(
    obj: &'a JsonMap<String, JsonValue>,
    name: &str,
    path: &str,
) -> Result<&'a JsonValue, DecodeError> {
    obj.get(name)
        .ok_or_else(|| invalid(&format!("{path}.{name}"), "missing required field"))
}

fn string<'a>(val: &'a JsonValue, path: &str) -> Result<&'a str, DecodeError> {
    val.as_str().ok_or_else(|| invalid(path, "expected string"))
}

fn integer(val: &JsonValue, path: &str) -> Result<i64, DecodeError> {
    val.as_i64()
        .ok_or_else(|| invalid(path, "expected signed 64-bit integer"))
}

fn number(val: &JsonValue, path: &str) -> Result<f64, DecodeError> {
    val.as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| invalid(path, "expected finite number"))
}

fn array<'a>(val: &'a JsonValue, path: &str) -> Result<&'a [JsonValue], DecodeError> {
    val.as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid(path, "expected array"))
}

fn keys(obj: &JsonMap<String, JsonValue>, allowed: &[&str], path: &str) -> Result<(), DecodeError> {
    if let Some(key) = obj.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(invalid(&format!("{path}.{key}"), "unrecognized field"));
    }
    Ok(())
}

/// Decode standalone persistence JSON (which follows version-2 tag rules).
pub fn try_json_to_cypher_value(val: &JsonValue) -> Result<CypherValue, DecodeError> {
    try_json_to_cypher_value_v2(val)
}

/// Decode version-2 snapshot values. Known tags are strict; the exact
/// `{"__type":"map","value":{...}}` form escapes ordinary maps.
///
/// ```
/// use cypherlite_model::{CypherValue, serde_value::{
///     cypher_value_to_json, try_json_to_cypher_value_v2,
///     try_json_to_cypher_value_legacy, try_json_to_param_value,
/// }};
/// use serde_json::json;
///
/// let point = json!({"__type":"point", "srid":4326, "x":1.5, "y":-2.25});
/// assert!(matches!(try_json_to_cypher_value_v2(&point)?, CypherValue::Point(_)));
/// // A v2 escaped map cannot be mistaken for a point.
/// let escaped = json!({"__type":"map", "value":{"__type":"point", "x":1}});
/// let map = try_json_to_cypher_value_v2(&escaped)?;
/// assert!(matches!(map, CypherValue::Map(_)));
/// assert_eq!(cypher_value_to_json(&map)?, escaped);
/// // In v1, __type=map is ordinary data; its nested values still use v1 rules.
/// let legacy_map = json!({"__type":"map", "value":{"x":1}});
/// let legacy = try_json_to_cypher_value_legacy(&legacy_map)?;
/// assert!(matches!(&legacy, CypherValue::Map(m) if m.contains_key("value")));
/// assert!(try_json_to_cypher_value_legacy(&escaped).is_err());
/// // User parameters never infer typed values from __type.
/// assert!(matches!(try_json_to_param_value(&point)?, CypherValue::Map(_)));
///
/// let invalid = json!({"__type":"point", "srid":4326, "x":1.5});
/// assert_eq!(try_json_to_cypher_value_v2(&invalid).unwrap_err().path, "$.y");
/// assert!(serde_json::from_value::<CypherValue>(invalid).is_err());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn try_json_to_cypher_value_v2(val: &JsonValue) -> Result<CypherValue, DecodeError> {
    decode(val, "$", DecodeMode::V2)
}

/// Decode version-1 snapshots. `__type=map` is ordinary map data in v1;
/// valid historical typed tags still have their original interpretation.
pub fn try_json_to_cypher_value_legacy(val: &JsonValue) -> Result<CypherValue, DecodeError> {
    decode(val, "$", DecodeMode::Legacy)
}

/// Decode user-supplied JSON parameters. Objects are always ordinary maps,
/// including those with `__type`; typed values require explicit query constructors.
pub fn try_json_to_param_value(val: &JsonValue) -> Result<CypherValue, DecodeError> {
    decode(val, "$", DecodeMode::Parameter)
}

/// Selects whether `__type` is interpreted as a persistence tag and whether
/// the v2 map envelope is recognized. Parameters treat every object as a map.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DecodeMode {
    Legacy,
    V2,
    Parameter,
}

fn decode_map(
    obj: &JsonMap<String, JsonValue>,
    path: &str,
    mode: DecodeMode,
) -> Result<CypherValue, DecodeError> {
    let mut result = IndexMap::new();
    for (key, value) in obj {
        let child = format!("{path}.{key}");
        result.insert(key.clone(), decode(value, &child, mode)?);
    }
    Ok(CypherValue::Map(result))
}

fn decode(val: &JsonValue, path: &str, mode: DecodeMode) -> Result<CypherValue, DecodeError> {
    match val {
        JsonValue::Null => Ok(CypherValue::Null),
        JsonValue::Bool(b) => Ok(CypherValue::Bool(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(CypherValue::Int(i))
            } else if n.is_u64() {
                Err(invalid(path, "integer exceeds i64 range"))
            } else {
                Ok(CypherValue::Float(number(val, path)?))
            }
        }
        JsonValue::String(s) => Ok(CypherValue::Str(s.clone())),
        JsonValue::Array(items) => items
            .iter()
            .enumerate()
            .map(|(i, item)| decode(item, &format!("{path}[{i}]"), mode))
            .collect::<Result<Vec<_>, _>>()
            .map(CypherValue::List),
        JsonValue::Object(obj) => {
            if mode == DecodeMode::Parameter {
                return decode_map(obj, path, mode);
            }
            let Some(tag) = obj.get("__type").and_then(JsonValue::as_str) else {
                return decode_map(obj, path, mode);
            };
            if tag == "map" && mode == DecodeMode::V2 {
                keys(obj, &["__type", "value"], path)?;
                let contents = field(obj, "value", path)?
                    .as_object()
                    .ok_or_else(|| invalid(&format!("{path}.value"), "expected object"))?;
                return decode_map(contents, path, mode);
            }
            let value = match tag {
                "date" => {
                    keys(obj, &["__type", "value"], path)?;
                    let s = string(field(obj, "value", path)?, &format!("{path}.value"))?;
                    CypherValue::Date(
                        s.parse::<NaiveDate>()
                            .map_err(|e| invalid(&format!("{path}.value"), e.to_string()))?,
                    )
                }
                "time" => {
                    keys(obj, &["__type", "value", "offset"], path)?;
                    let s = string(field(obj, "value", path)?, &format!("{path}.value"))?;
                    let t = NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
                        .map_err(|e| invalid(&format!("{path}.value"), e.to_string()))?;
                    let offset =
                        obj.get("offset")
                            .map(|o| {
                                parse_numeric_offset(string(o, &format!("{path}.offset"))?)
                                    .ok_or_else(|| {
                                        invalid(
                                            &format!("{path}.offset"),
                                            "expected ASCII ±HH:MM offset within 24 hours",
                                        )
                                    })
                            })
                            .transpose()?;
                    CypherValue::Time(t, offset)
                }
                "datetime" => {
                    keys(obj, &["__type", "value", "timezone"], path)?;
                    let s = string(field(obj, "value", path)?, &format!("{path}.value"))?;
                    let dt = parse_persistence_datetime(s)
                        .map_err(|e| invalid(&format!("{path}.value"), e.to_string()))?;
                    let tz = obj
                        .get("timezone")
                        .map(|v| string(v, &format!("{path}.timezone")).map(str::to_owned))
                        .transpose()?;
                    CypherValue::DateTime(dt, tz)
                }
                "localdatetime" => {
                    keys(obj, &["__type", "value"], path)?;
                    let s = string(field(obj, "value", path)?, &format!("{path}.value"))?;
                    CypherValue::LocalDateTime(
                        NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                            .map_err(|e| invalid(&format!("{path}.value"), e.to_string()))?,
                    )
                }
                "duration" => {
                    keys(
                        obj,
                        &["__type", "months", "days", "seconds", "nanoseconds"],
                        path,
                    )?;
                    let component =
                        |name: &str| integer(field(obj, name, path)?, &format!("{path}.{name}"));
                    CypherValue::Duration(IsoDuration::new(
                        component("months")?,
                        component("days")?,
                        component("seconds")?,
                        component("nanoseconds")?,
                    ))
                }
                "point" => {
                    keys(obj, &["__type", "srid", "x", "y", "z"], path)?;
                    let srid =
                        i32::try_from(integer(field(obj, "srid", path)?, &format!("{path}.srid"))?)
                            .map_err(|_| {
                                invalid(&format!("{path}.srid"), "SRID exceeds i32 range")
                            })?;
                    let coordinate =
                        |name: &str| number(field(obj, name, path)?, &format!("{path}.{name}"));
                    let z = obj
                        .get("z")
                        .map(|v| number(v, &format!("{path}.z")))
                        .transpose()?;
                    CypherValue::Point(PointValue {
                        srid,
                        x: coordinate("x")?,
                        y: coordinate("y")?,
                        z,
                    })
                }
                "geometry" => CypherValue::Geometry(decode_geometry(obj, path, mode)?),
                "vector" => decode_vector(obj, path)?,
                _ => return decode_map(obj, path, mode),
            };
            Ok(value)
        }
    }
}

fn decode_vector(obj: &JsonMap<String, JsonValue>, path: &str) -> Result<CypherValue, DecodeError> {
    keys(obj, &["__type", "coord", "dim", "values"], path)?;
    let coord_path = format!("{path}.coord");
    let coord =
        VectorCoordKind::from_canonical_name(string(field(obj, "coord", path)?, &coord_path)?)
            .ok_or_else(|| invalid(&coord_path, "unknown vector coordinate type"))?;
    let dim_path = format!("{path}.dim");
    let dim = usize::try_from(
        field(obj, "dim", path)?
            .as_u64()
            .ok_or_else(|| invalid(&dim_path, "expected nonnegative integer dimension"))?,
    )
    .map_err(|_| invalid(&dim_path, "dimension exceeds usize range"))?;
    let values_path = format!("{path}.values");
    let items = array(field(obj, "values", path)?, &values_path)?;
    if dim != items.len() {
        return Err(invalid(&dim_path, "dimension does not match values length"));
    }
    let mut values = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let at = format!("{values_path}[{i}]");
        let value = match coord {
            VectorCoordKind::Int8 => CypherValue::Int(i64::from(
                i8::try_from(integer(item, &at)?)
                    .map_err(|_| invalid(&at, "INTEGER8 out of range"))?,
            )),
            VectorCoordKind::Int16 => CypherValue::Int(i64::from(
                i16::try_from(integer(item, &at)?)
                    .map_err(|_| invalid(&at, "INTEGER16 out of range"))?,
            )),
            VectorCoordKind::Int32 => CypherValue::Int(i64::from(
                i32::try_from(integer(item, &at)?)
                    .map_err(|_| invalid(&at, "INTEGER32 out of range"))?,
            )),
            VectorCoordKind::Int64 => CypherValue::Int(integer(item, &at)?),
            VectorCoordKind::Float32 => {
                let f = float_number(item, &at)?;
                if !float32_exact(f) {
                    return Err(invalid(&at, "FLOAT32 out of range or loses precision"));
                }
                CypherValue::Float(f)
            }
            VectorCoordKind::Float64 => CypherValue::Float(float_number(item, &at)?),
        };
        values.push(value);
    }
    Ok(CypherValue::Vector { coord, dim, values })
}

fn float_number(val: &JsonValue, path: &str) -> Result<f64, DecodeError> {
    if !val.is_f64() {
        return Err(invalid(path, "expected floating-point number"));
    }
    number(val, path)
}

fn decode_geometry(
    obj: &JsonMap<String, JsonValue>,
    path: &str,
    mode: DecodeMode,
) -> Result<GeometryValue, DecodeError> {
    let srid = i32::try_from(integer(field(obj, "srid", path)?, &format!("{path}.srid"))?)
        .map_err(|_| invalid(&format!("{path}.srid"), "SRID exceeds i32 range"))?;
    let kind = string(
        field(obj, "geometry_type", path)?,
        &format!("{path}.geometry_type"),
    )?;
    let coords_path = format!("{path}.coordinates");
    let geometry = match kind {
        "LineString" | "MultiPoint" => {
            keys(
                obj,
                &["__type", "geometry_type", "srid", "coordinates"],
                path,
            )?;
            let coords = decode_coords(field(obj, "coordinates", path)?, &coords_path)?;
            if kind == "LineString" {
                GeometryValue::LineString {
                    coordinates: coords,
                    srid,
                }
            } else {
                GeometryValue::MultiPoint {
                    points: coords,
                    srid,
                }
            }
        }
        "Polygon" | "MultiLineString" => {
            keys(
                obj,
                &["__type", "geometry_type", "srid", "coordinates"],
                path,
            )?;
            let rings = decode_nested_coords(field(obj, "coordinates", path)?, &coords_path)?;
            if kind == "Polygon" {
                GeometryValue::Polygon { rings, srid }
            } else {
                GeometryValue::MultiLineString { lines: rings, srid }
            }
        }
        "MultiPolygon" => {
            keys(
                obj,
                &["__type", "geometry_type", "srid", "coordinates"],
                path,
            )?;
            let polygons = array(field(obj, "coordinates", path)?, &coords_path)?
                .iter()
                .enumerate()
                .map(|(i, v)| decode_nested_coords(v, &format!("{coords_path}[{i}]")))
                .collect::<Result<_, _>>()?;
            GeometryValue::MultiPolygon { polygons, srid }
        }
        "GeometryCollection" => {
            keys(
                obj,
                &["__type", "geometry_type", "srid", "geometries"],
                path,
            )?;
            let geometries = array(
                field(obj, "geometries", path)?,
                &format!("{path}.geometries"),
            )?
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let at = format!("{path}.geometries[{i}]");
                match decode(item, &at, mode)? {
                    CypherValue::Geometry(g) => Ok(g),
                    _ => Err(invalid(&at, "expected tagged geometry")),
                }
            })
            .collect::<Result<_, _>>()?;
            GeometryValue::GeometryCollection { geometries, srid }
        }
        _ => {
            return Err(invalid(
                &format!("{path}.geometry_type"),
                "unknown geometry type",
            ))
        }
    };
    Ok(geometry)
}

fn decode_nested_coords(val: &JsonValue, path: &str) -> Result<Vec<Vec<Coord2D>>, DecodeError> {
    array(val, path)?
        .iter()
        .enumerate()
        .map(|(i, v)| decode_coords(v, &format!("{path}[{i}]")))
        .collect()
}

fn decode_coords(val: &JsonValue, path: &str) -> Result<Vec<Coord2D>, DecodeError> {
    array(val, path)?
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let at = format!("{path}[{i}]");
            let pair = array(v, &at)?;
            if pair.len() != 2 {
                return Err(invalid(&at, "expected exactly two coordinates"));
            }
            Ok([
                number(&pair[0], &format!("{at}[0]"))?,
                number(&pair[1], &format!("{at}[1]"))?,
            ])
        })
        .collect()
}

/// Convert a CypherValue property map to a serde_json::Map, retaining key paths.
pub fn props_to_json(
    props: &IndexMap<String, CypherValue>,
) -> Result<JsonMap<String, JsonValue>, EncodeError> {
    encode_map(props, "$")
}

/// Decode standalone property values following version-2 tag rules.
pub fn try_json_to_props(
    obj: &JsonMap<String, JsonValue>,
) -> Result<IndexMap<String, CypherValue>, DecodeError> {
    try_json_to_props_v2(obj)
}

/// Decode version-2 snapshot properties, retaining property-key error paths.
pub fn try_json_to_props_v2(
    obj: &JsonMap<String, JsonValue>,
) -> Result<IndexMap<String, CypherValue>, DecodeError> {
    decode_props(obj, DecodeMode::V2)
}

/// Decode version-1 snapshot properties (where `__type=map` was ordinary data).
pub fn try_json_to_props_legacy(
    obj: &JsonMap<String, JsonValue>,
) -> Result<IndexMap<String, CypherValue>, DecodeError> {
    decode_props(obj, DecodeMode::Legacy)
}

fn decode_props(
    obj: &JsonMap<String, JsonValue>,
    mode: DecodeMode,
) -> Result<IndexMap<String, CypherValue>, DecodeError> {
    obj.iter()
        .map(|(key, value)| Ok((key.clone(), decode(value, &format!("$.{key}"), mode)?)))
        .collect()
}

// ---- serde::Serialize / Deserialize impls for CypherValue ----

impl serde::Serialize for CypherValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let json = cypher_value_to_json(self).map_err(serde::ser::Error::custom)?;
        json.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for CypherValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let json = JsonValue::deserialize(deserializer)?;
        try_json_to_cypher_value(&json).map_err(serde::de::Error::custom)
    }
}

// chrono's RFC 3339 parser rejects expanded years (e.g. +262142) that its
// to_rfc3339 writer emits. Accept only that exact canonical writer form.
fn parse_persistence_datetime(s: &str) -> Result<DateTime<FixedOffset>, chrono::ParseError> {
    match DateTime::parse_from_rfc3339(s) {
        Ok(dt) => Ok(dt),
        Err(original) => {
            let bytes = s.as_bytes();
            let year_end = 1 + bytes
                .iter()
                .skip(1)
                .take_while(|b| b.is_ascii_digit())
                .count();
            if !matches!(bytes.first(), Some(b'+' | b'-'))
                || year_end < 6
                || bytes.get(year_end) != Some(&b'-')
            {
                return Err(original);
            }
            let dt = DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f%:z")?;
            if dt
                .naive_utc()
                .checked_add_signed(Duration::seconds(i64::from(dt.offset().local_minus_utc())))
                .is_some()
                && dt.to_rfc3339() == s
            {
                Ok(dt)
            } else {
                Err(original)
            }
        }
    }
}

/// Parse exactly six ASCII bytes in signed `±HH:MM` form into a fixed offset.
///
/// The sign, two hour digits, colon, and two minute digits are required. Minutes
/// must be at most 59, and the resulting offset must fit chrono's `FixedOffset`
/// range. Returns `None` for malformed or out-of-range input, including
/// non-ASCII text, without panicking.
///
/// ```
/// use cypherlite_model::serde_value::parse_numeric_offset;
///
/// assert_eq!(parse_numeric_offset("+02:30").unwrap().local_minus_utc(), 9_000);
/// assert_eq!(parse_numeric_offset("-05:45").unwrap().local_minus_utc(), -20_700);
/// assert!(parse_numeric_offset("+00:60").is_none());
/// assert!(parse_numeric_offset("é+02:30").is_none());
/// ```
pub fn parse_numeric_offset(s: &str) -> Option<FixedOffset> {
    let bytes = s.as_bytes();
    if bytes.len() != 6 || !bytes.is_ascii() || bytes[3] != b':' {
        return None;
    }
    let sign: i32 = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digit = |b: u8| -> Option<i32> { b.is_ascii_digit().then(|| i32::from(b - b'0')) };
    let h = digit(bytes[1])?
        .checked_mul(10)?
        .checked_add(digit(bytes[2])?)?;
    let m = digit(bytes[4])?
        .checked_mul(10)?
        .checked_add(digit(bytes[5])?)?;
    if m > 59 {
        return None;
    }
    let seconds = h
        .checked_mul(3600)?
        .checked_add(m.checked_mul(60)?)?
        .checked_mul(sign)?;
    FixedOffset::east_opt(seconds)
}

#[cfg(test)]
mod tests;
