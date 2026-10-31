//! Conversion builtins: `toString`/`toInteger`/`toFloat`/`toBoolean` and
//! their `OrNull` variants, the `toFloatList`/`toIntegerList` list
//! conversions, plus the shared coercion helpers (CIP-271's dynamic
//! `LIST<ANY>` acceptance rule lives here). String → number parsing lives in
//! the [`string`] submodule (ADR-0175).

use std::borrow::Cow;

use cypherlite_core::{Error, Result, Value};

use crate::eval::numeric::float_to_i64;
use crate::overlay::ExecGraph;

use super::super::temporal::temporal_full_seconds;
use super::coerce_scalar_to_string;
use string::{float_from_string, integer_from_string, IntegerString};

mod string;

pub(super) fn bi_tostring(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    to_string_value(vals.first())
}

pub(super) fn bi_tostring_full(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    to_string_full(vals.first())
}

pub(super) fn bi_tostringornull(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    Ok(to_string_value(vals.first()).unwrap_or(Value::Null))
}
pub(super) fn bi_toint(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    to_integer_value(vals.first())
}

pub(super) fn bi_tointegerornull(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    Ok(to_integer_value(vals.first()).unwrap_or(Value::Null))
}

pub(super) fn bi_tointegerlist(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    convert_list(&vals, ListProjection::Integer)
}

pub(super) fn bi_tofloat(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    to_float_value(vals.first())
}

pub(super) fn bi_tofloatornull(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    Ok(to_float_value(vals.first()).unwrap_or(Value::Null))
}

pub(super) fn bi_tofloatlist(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    convert_list(&vals, ListProjection::Float)
}
pub(super) fn bi_tobool(_graph: &ExecGraph<'_>, _n: &str, vals: Vec<Value>) -> Result<Value> {
    to_boolean_value(vals.first())
}

pub(super) fn bi_tobooleanornull(
    _graph: &ExecGraph<'_>,
    _n: &str,
    vals: Vec<Value>,
) -> Result<Value> {
    Ok(to_boolean_value(vals.first()).unwrap_or(Value::Null))
}

/// The element projection a list conversion applies: `toIntegerList()`
/// truncates to integers, `toFloatList()` widens to floats.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum ListProjection {
    Integer,
    Float,
}

/// Shared body of `toIntegerList()`/`toFloatList()`: convert a list or
/// vector's elements per `projection`. A VECTOR's coordinates are first
/// normalised to FLOAT elements, so they always convert through FLOAT, as in
/// Neo4j (ADR-0168); a list's elements are projected as they are by
/// [`convert_element`].
fn convert_list(vals: &[Value], projection: ListProjection) -> Result<Value> {
    let Some(value) = vals.first() else {
        return Ok(Value::Null);
    };
    if value.is_null() {
        return Ok(Value::Null);
    }
    let coerce_float = |v: &Value| v.as_number().map(Value::Float).unwrap_or(Value::Null);
    let items: Cow<'_, [Value]> = match value {
        Value::List(xs) => Cow::Borrowed(xs.as_slice()),
        Value::Vector { values, .. } => Cow::Owned(values.iter().map(coerce_float).collect()),
        _ => return Err(Error::ty("list conversion requires a list or vector")),
    };
    let converted = items
        .iter()
        .map(|v| convert_element(v, projection))
        .collect::<Vec<_>>();
    Ok(Value::List(converted.into()))
}

/// One list element of `toIntegerList()`/`toFloatList()`, converted with the
/// scalar `OrNull` rules (ADR-0175, mirroring Neo4j's `CypherFunctions`,
/// which routes every element through the `OrNull` paths): null stays null;
/// a FLOAT element converts per ADR-0168 ([`float_to_i64`]: NaN → 0,
/// saturate, truncate toward zero); a STRING element parses with the same
/// string parser as `toIntegerOrNull()`/`toFloatOrNull()`
/// ([`integer_from_string`]/[`float_from_string`]) — a string that fails to
/// parse, including an out-of-range numeric string, becomes a NULL element
/// and never raises; a BOOLEAN element converts to 1/0 in `toIntegerList()`
/// and to null in `toFloatList()`; every other element type coerces to null.
fn convert_element(v: &Value, projection: ListProjection) -> Value {
    match (projection, v) {
        (_, Value::Null) => Value::Null,
        (ListProjection::Integer, Value::Int(i)) => Value::Int(*i),
        (ListProjection::Integer, Value::Float(f)) => Value::Int(float_to_i64(*f)),
        (ListProjection::Integer, Value::Bool(b)) => Value::Int(i64::from(*b)),
        (ListProjection::Integer, Value::String(s)) => match integer_from_string(s) {
            IntegerString::InRange(i) => Value::Int(i),
            IntegerString::OutOfRange | IntegerString::NotANumber => Value::Null,
        },
        (ListProjection::Float, Value::Float(f)) => Value::Float(*f),
        (ListProjection::Float, Value::Int(i)) => Value::Float(*i as f64),
        (ListProjection::Float, Value::String(s)) => {
            float_from_string(s).map_or(Value::Null, Value::Float)
        }
        _ => Value::Null,
    }
}

/// CIP-271: the type class a value belongs to for the `LIST<ANY>`
/// acceptance rule. INTEGER and FLOAT share one class; null is filtered out
/// by [`list_tostring_accepted`] before this is called.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum TypeClass {
    Number,
    String,
    Bool,
    List,
    Map,
    Node,
    Relationship,
    Path,
    Temporal,
    Point,
    Uuid,
    Vector,
    Null,
}

fn type_class(v: &Value) -> TypeClass {
    match v {
        Value::Int(_) | Value::Float(_) => TypeClass::Number,
        Value::String(_) => TypeClass::String,
        Value::Bool(_) => TypeClass::Bool,
        // A byte array types as LIST<INTEGER> (ADR-0327).
        Value::List(_) | Value::Bytes(_) => TypeClass::List,
        Value::Map(_) => TypeClass::Map,
        Value::Node(_) => TypeClass::Node,
        Value::Relationship(_) => TypeClass::Relationship,
        Value::Path(_) => TypeClass::Path,
        Value::Temporal(_) => TypeClass::Temporal,
        Value::Point(_) => TypeClass::Point,
        // CIP-246
        Value::Uuid(_) => TypeClass::Uuid,
        Value::Vector { .. } => TypeClass::Vector,
        Value::Null => TypeClass::Null,
    }
}

/// CIP-271: Neo4j's static typing accepts `toString()` on `LIST<ANY>` (or a
/// dynamic union) and rejects homogeneous lists (`LIST<T>` for a concrete
/// `T`, with INTEGER and FLOAT unifying). Emulated dynamically: a list is
/// accepted when it is empty, contains a null (ANY), or holds elements of
/// more than one type class — numbers counting as a single class.
fn list_tostring_accepted(xs: &[Value]) -> bool {
    if xs.is_empty() || xs.iter().any(|v| matches!(v, Value::Null)) {
        return true;
    }
    let first = type_class(&xs[0]);
    xs.iter().any(|v| type_class(v) != first)
}

/// Shared body of `toString()`/`toStringOrNull()`: convert an optional value
/// to its string form. Null and absent input yield `Value::Null`; only a
/// non-convertible *type* (a homogeneous list) errors, which the `OrNull`
/// variant maps to null.
fn to_string_value(v: Option<&Value>) -> Result<Value> {
    match v {
        Some(Value::Null) | None => Ok(Value::Null),
        Some(v) => match v {
            // CIP-271: toString() renders maps, heterogeneous lists,
            // and graph values in display form. A homogeneous list
            // (every non-null element of one type class, numbers one
            // class) is still a type error — that emulates Neo4j's
            // static LIST<ANY>-only acceptance dynamically (verified
            // 2026.09.0: [1,'a'] ok; [1,2] / ['a','b'] / [1,2.0] /
            // [[1]] reject; any list containing null ok).
            // The result is the `to_display_string()` form except for
            // two cases: the homogeneous-list type error, and a Vector,
            // which renders in its literal form. `to_string_full`
            // overrides the temporal rendering on top of this body. A
            // STRING is returned as the same shared payload rather than a
            // copy.
            Value::List(xs) if list_tostring_accepted(xs) => {
                Ok(Value::String(v.to_display_string().into()))
            }
            // A byte array is a homogeneous LIST<INTEGER> (ADR-0327).
            Value::List(_) | Value::Bytes(_) => Err(Error::ty("InvalidArgumentValue: toString()")),
            // Maps and graph values render in display form (CIP-271).
            Value::Map(_) | Value::Node(_) | Value::Relationship(_) | Value::Path(_) => {
                Ok(Value::String(v.to_display_string().into()))
            }
            // Every scalar, STRING included, goes through the coercion
            // helper shared with CIP-261; anything it declines (a Vector)
            // falls back to its literal form.
            _ => Ok(Value::String(
                coerce_scalar_to_string(v).unwrap_or_else(|| v.to_cypher_literal().into()),
            )),
        },
    }
}

/// `toString()` with full temporal rendering: a temporal renders with
/// full-precision seconds; every other value goes through
/// [`to_string_value`] unchanged.
fn to_string_full(v: Option<&Value>) -> Result<Value> {
    match v {
        Some(Value::Temporal(t)) => Ok(Value::String(temporal_full_seconds(t).into())),
        _ => to_string_value(v),
    }
}

fn to_boolean_value(v: Option<&Value>) -> Result<Value> {
    match v {
        Some(Value::Bool(b)) => Ok(Value::Bool(*b)),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => Ok(Value::Bool(true)),
        Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => Ok(Value::Bool(false)),
        Some(Value::String(_)) | Some(Value::Null) | None => Ok(Value::Null),
        _ => Err(Error::ty("InvalidArgumentValue: toBoolean()")),
    }
}

/// Shared body of `toInteger()`/`toIntegerOrNull()` (ADR-0168, string rules
/// ADR-0175): floats go through [`float_to_i64`] and never fail; a
/// string goes through [`integer_from_string`] — Neo4j's `parseLong` →
/// exact-decimal → Cypher25-literal-fallback pipeline. An out-of-range
/// exact decimal is a `NumberOutOfRange` error, which the `OrNull` variant
/// maps to null; a non-numeric string (every NaN and Infinity spelling
/// included) is null.
fn to_integer_value(v: Option<&Value>) -> Result<Value> {
    Ok(match v {
        Some(Value::Int(i)) => Value::Int(*i),
        Some(Value::Float(f)) => Value::Int(float_to_i64(*f)),
        Some(Value::String(s)) => match integer_from_string(s) {
            IntegerString::InRange(i) => Value::Int(i),
            IntegerString::OutOfRange => {
                return Err(Error::ty(format!(
                    "NumberOutOfRange: toInteger() expected 'input' to be of type INTEGER \
                     and in the range {} to {} but found {}",
                    i64::MIN,
                    i64::MAX,
                    string::trim_java(s)
                )))
            }
            IntegerString::NotANumber => Value::Null,
        },
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("InvalidArgumentValue: toInteger()")),
    })
}

/// Shared body of `toFloat()`/`toFloatOrNull()` (string rules ADR-0175):
/// convert an optional value to a float. A string goes through
/// [`float_from_string`] — Neo4j's `Double.parseDouble` + Cypher25-literal
/// pipeline. Parse failures and null return `Value::Null`; only a
/// non-convertible *type* errors, which the `OrNull` variant maps to null.
fn to_float_value(v: Option<&Value>) -> Result<Value> {
    Ok(match v {
        Some(Value::Float(f)) => Value::Float(*f),
        Some(Value::Int(i)) => Value::Float(*i as f64),
        Some(Value::String(s)) => float_from_string(s).map_or(Value::Null, Value::Float),
        Some(Value::Null) | None => Value::Null,
        _ => return Err(Error::ty("InvalidArgumentValue: toFloat()")),
    })
}

#[cfg(test)]
mod tests;
