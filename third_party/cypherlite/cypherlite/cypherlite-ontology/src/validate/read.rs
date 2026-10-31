//! The shared read layer behind the document validators (ADR-0364 §2).
//!
//! Every accessor here is one dispatch shape with one error sentence, so the
//! document/clause validators in [`super`] and the extension payload checks
//! in [`super::ext`] only name a field and a reader. One scalar combinator
//! ([`read_scalar`]) carries the absent/present policy and one array walk
//! ([`read_list`]) the expect-array/indexed-item policy; the scalar family
//! is `req_*` (absent is the missing-field error), `opt_*` (absent is
//! `None`, `null` is a type error) and `opt_nullable_*` (absent and `null`
//! are both `None`), and the array and sub-value families are the same
//! three shapes parameterized by an item or sub-validator. The Neo4j token
//! readers are the same family over the closed [`Neo4jType`] vocabulary,
//! whose expected list derives from [`NEO4J_TYPES`].
//!
//! These items stay inside the `validate` module tree (`pub(super)`): they
//! are an internal decomposition, not public surface. Error paths and
//! messages are behavior the acceptance suite pins, so every helper keeps
//! the exact sentence of the call site it replaced.

use indexmap::IndexMap;

use crate::error::OntologyError;
use crate::model::{Neo4jType, NEO4J_TYPES};
use crate::value::Value;

pub(super) fn verr(path: &str, message: impl Into<String>) -> OntologyError {
    OntologyError::Validation {
        path: path.to_owned(),
        message: message.into(),
    }
}

pub(super) fn join(path: &str, key: &str) -> String {
    if path == "/" {
        format!("/{key}")
    } else {
        format!("{path}/{key}")
    }
}

pub(super) fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Boolean(_) => "a boolean",
        Value::Integer(_) => "an integer",
        Value::Float(_) => "a float",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

pub(super) fn expect_object<'a>(
    value: &'a Value,
    path: &str,
    what: &str,
) -> Result<&'a IndexMap<String, Value>, OntologyError> {
    value.as_object().ok_or_else(|| {
        verr(
            path,
            format!("{what} must be an object, found {}", kind_of(value)),
        )
    })
}

pub(super) fn check_allowed(
    obj: &IndexMap<String, Value>,
    allowed: &[&str],
    path: &str,
) -> Result<(), OntologyError> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(verr(
                &join(path, key),
                format!("unknown field '{key}' (additional properties are not allowed here)"),
            ));
        }
    }
    Ok(())
}

/// The one absent-required sentence: every required reader — scalar, array
/// or sub-value — reports it at the holder's `path`, naming the field.
fn missing_field(path: &str, key: &str) -> OntologyError {
    verr(path, format!("missing required field '{key}'"))
}

/// The one scalar presence policy: absent → `None`; present → `parse` at
/// the field's own path, whose miss reports `'<key>' must be <expected>,
/// found <kind>` at `<path>/<key>`. Every `opt_*` scalar is this function,
/// and every `req_*` scalar is this function plus [`missing_field`], so no
/// accessor restates an absent or wrong-kind arm. `expected` is the full
/// article phrase ("a string", "a number"), keeping each sentence exact.
fn read_scalar<T>(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
    expected: &str,
    parse: impl FnOnce(&Value) -> Option<T>,
) -> Result<Option<T>, OntologyError> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => parse(v).map(Some).ok_or_else(|| {
            verr(
                &join(path, key),
                format!("'{key}' must be {expected}, found {}", kind_of(v)),
            )
        }),
    }
}

pub(super) fn req_string(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<String, OntologyError> {
    read_scalar(obj, key, path, "a string", |v| {
        v.as_str().map(str::to_owned)
    })?
    .ok_or_else(|| missing_field(path, key))
}

pub(super) fn opt_string(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<String>, OntologyError> {
    read_scalar(obj, key, path, "a string", |v| {
        v.as_str().map(str::to_owned)
    })
}

/// The one nullable-scalar shape: absent and `null` both mean "no value";
/// any other value is read by the scalar's `read`, and a miss reports the
/// shared `must be <expected> or null` message. Every `opt_nullable_*`
/// accessor is this function with one scalar reader plugged in. Only the
/// `null` arm is stated here — the absent and wrong-kind arms are
/// [`read_scalar`]'s.
fn opt_nullable<T>(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
    expected: &str,
    read: impl FnOnce(&Value) -> Option<T>,
) -> Result<Option<T>, OntologyError> {
    match obj.get(key) {
        Some(Value::Null) => Ok(None),
        _ => read_scalar(obj, key, path, &format!("{expected} or null"), read),
    }
}

/// A nullable string: absent and `null` both mean "no value"; any other
/// non-string is a type error on that field.
pub(super) fn opt_nullable_string(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<String>, OntologyError> {
    opt_nullable(obj, key, path, "a string", |v| {
        v.as_str().map(str::to_owned)
    })
}

pub(super) fn opt_bool(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<bool>, OntologyError> {
    read_scalar(obj, key, path, "a boolean", Value::as_boolean)
}

pub(super) fn opt_nullable_bool(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<bool>, OntologyError> {
    opt_nullable(obj, key, path, "a boolean", Value::as_boolean)
}

pub(super) fn opt_int(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<i64>, OntologyError> {
    read_scalar(obj, key, path, "an integer", Value::as_integer)
}

/// A nullable integer with no minimum: absent and `null` mean "no value".
pub(super) fn opt_nullable_int(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<i64>, OntologyError> {
    opt_nullable(obj, key, path, "an integer", Value::as_integer)
}

/// A nullable number: absent and `null` both mean "no value".
pub(super) fn opt_nullable_number(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<f64>, OntologyError> {
    opt_nullable(obj, key, path, "a number", as_number)
}

pub(super) fn req_number(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<f64, OntologyError> {
    read_scalar(obj, key, path, "a number", as_number)?.ok_or_else(|| missing_field(path, key))
}

/// A JSON `number` (integer or float), with its field's error sentence.
fn number(value: &Value, key: &str, path: &str) -> Result<f64, OntologyError> {
    as_number(value).ok_or_else(|| {
        verr(
            path,
            format!("'{key}' must be a number, found {}", kind_of(value)),
        )
    })
}

/// The `number` scalar (integer or float) of a value, if it is one.
fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

pub(super) fn opt_string_list(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Vec<String>, OntologyError> {
    match obj.get(key) {
        None => Ok(Vec::new()),
        Some(v) => string_list(v, key, &join(path, key)),
    }
}

/// An optional array at `key`: absent → `None`, present → the item
/// reader at `<path>/<key>`. The absent→`None` array arm beside
/// [`opt_string_list`] (absent is the empty list); parameterized by the
/// item reader so every array kind shares the one dispatch.
pub(super) fn opt_list<T>(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
    read: impl FnOnce(&Value, &str, &str) -> Result<Vec<T>, OntologyError>,
) -> Result<Option<Vec<T>>, OntologyError> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => read(v, key, &join(path, key)).map(Some),
    }
}

pub(super) fn req_string_list(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Vec<String>, OntologyError> {
    match obj.get(key) {
        None => Err(missing_field(path, key)),
        Some(v) => string_list(v, key, &join(path, key)),
    }
}

/// The one array walk: a non-array fails at `path` with `'<key>' must be
/// an array, found <kind>`, and item `i` is parsed at `<path>/<i>` by
/// `parse_item`, which owns its family's item sentence. Every `*_list`
/// reader is this walk with one item reader plugged in.
fn read_list<T>(
    value: &Value,
    key: &str,
    path: &str,
    parse_item: impl Fn(&Value, &str) -> Result<T, OntologyError>,
) -> Result<Vec<T>, OntologyError> {
    let items = value.as_array().ok_or_else(|| {
        verr(
            path,
            format!("'{key}' must be an array, found {}", kind_of(value)),
        )
    })?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        out.push(parse_item(item, &join(path, &i.to_string()))?);
    }
    Ok(out)
}

/// One string-shaped item of a list at `at`: the shared item boundary for
/// the string list readers. A non-string reports `'<key>' items must be
/// <expected>, found <kind>` at the item's path.
fn string_item<'a>(
    item: &'a Value,
    key: &str,
    at: &str,
    expected: &str,
) -> Result<&'a str, OntologyError> {
    item.as_str().ok_or_else(|| {
        verr(
            at,
            format!("'{key}' items must be {expected}, found {}", kind_of(item)),
        )
    })
}

pub(super) fn string_list(
    value: &Value,
    key: &str,
    path: &str,
) -> Result<Vec<String>, OntologyError> {
    read_list(value, key, path, |item, at| {
        Ok(string_item(item, key, at, "strings")?.to_owned())
    })
}

pub(super) fn number_list(value: &Value, key: &str, path: &str) -> Result<Vec<f64>, OntologyError> {
    read_list(value, key, path, |item, at| number(item, key, at))
}

/// An optional sub-value: absent → `T`'s default, present → `validate`
/// at `<path>/<key>`. The sub-value arm of the `opt_*`/`req_*` family:
/// every dispatch to a sub-validator (node/relationship/extension
/// entries, properties, constraints, labels) routes through this,
/// [`opt_nullable_sub`] or [`req_sub`] — no call site open-codes the
/// `match obj.get(..)` dispatch. A site that needs the present/absent
/// distinction passes `T = Option<_>`, defaulting to `None`.
pub(super) fn opt_sub<T: Default>(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
    validate: impl FnOnce(&Value, &str) -> Result<T, OntologyError>,
) -> Result<T, OntologyError> {
    match obj.get(key) {
        None => Ok(T::default()),
        Some(v) => validate(v, &join(path, key)),
    }
}

/// A nullable sub-value: absent and `null` both skip, present → `validate`
/// at `<path>/<key>`. The null-aware sub-value arm, beside [`opt_sub`].
pub(super) fn opt_nullable_sub(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
    validate: impl FnOnce(&Value, &str) -> Result<(), OntologyError>,
) -> Result<(), OntologyError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(()),
        Some(v) => validate(v, &join(path, key)),
    }
}

/// A required sub-value: absent → the missing-field error (the same
/// shape `req_string` reports for a string field), present → `validate`
/// at `<path>/<key>`.
pub(super) fn req_sub<T>(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
    validate: impl FnOnce(&Value, &str) -> Result<T, OntologyError>,
) -> Result<T, OntologyError> {
    match obj.get(key) {
        None => Err(missing_field(path, key)),
        Some(v) => validate(v, &join(path, key)),
    }
}

/// The one expected-token message for [`Neo4jType`] fields: the vocabulary
/// and its length both come from [`NEO4J_TYPES`], so the message never
/// drifts from the closed enum.
pub(super) fn expected_neo4j_types() -> String {
    let vocabulary = NEO4J_TYPES
        .iter()
        .map(|(token, _)| *token)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "expected one of main's {} Neo4jType tokens ({vocabulary})",
        NEO4J_TYPES.len()
    )
}

/// The one `Neo4jType` token reader: a string outside the closed vocabulary
/// fails at `path` naming the offending token and the derived vocabulary.
fn neo4j_type(token: &str, path: &str) -> Result<Neo4jType, OntologyError> {
    Neo4jType::parse(token).ok_or_else(|| {
        verr(
            path,
            format!(
                "invalid Neo4jType token '{token}': {}",
                expected_neo4j_types()
            ),
        )
    })
}

/// An array of `Neo4jType` tokens in the [`string_list`]/[`number_list`]
/// shape: the family's one [`read_list`] walk, with item `i` reporting at
/// `<path>/<i>` before its token is parsed.
pub(super) fn neo4j_type_list(
    value: &Value,
    key: &str,
    path: &str,
) -> Result<Vec<Neo4jType>, OntologyError> {
    read_list(value, key, path, |item, at| {
        neo4j_type(string_item(item, key, at, "Neo4jType strings")?, at)
    })
}

/// An optional `Neo4jType` token at `key`: the string boundary is
/// [`opt_string`]'s (absent → `None`, any other kind → the field's type
/// error), and only the token is parsed. The one reader behind the
/// property `type` and the extension `suggested` fields.
pub(super) fn opt_neo4j_type(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<Neo4jType>, OntologyError> {
    match opt_string(obj, key, path)? {
        None => Ok(None),
        Some(token) => neo4j_type(&token, &join(path, key)).map(Some),
    }
}

/// An optional array of `Neo4jType` tokens at `key` (absent is `None`), for
/// the extension `supported` field. Item `i` reports at `<path>/<key>/<i>`.
pub(super) fn opt_neo4j_type_list(
    obj: &IndexMap<String, Value>,
    key: &str,
    path: &str,
) -> Result<Option<Vec<Neo4jType>>, OntologyError> {
    opt_list(obj, key, path, neo4j_type_list)
}
