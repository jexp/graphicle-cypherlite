//! Canonical serialization of the value model (ADR-0227 §3): compact UTF-8
//! JSON and block-style YAML with two-space indentation, both preserving
//! the document's key order.
//!
//! ADR-0226 additionally needs canonical JSON *text* for free-form payloads
//! (`one_of` entries, extension payloads) that is stable across JSON/YAML
//! submissions of the same logical document: compact with object keys
//! sorted ([`write_json_sorted`]).

use std::borrow::Cow;
use std::fmt::Write as _;

use saphyr::{Mapping, Scalar, ScalarStyle, Yaml, YamlEmitter};

use crate::error::OntologyError;
use crate::value::Value;

/// The serialization depth limit, mirroring the parse screen's 128-level
/// cap. Parsed documents are pre-bounded by the screen; this bound exists
/// for hand-built [`Value`] trees, which are publicly constructible and
/// would otherwise drive the (recursive) JSON writer, the YAML node
/// builder and saphyr's emitter past the stack with no typed failure.
pub(crate) const MAX_SERIALIZE_DEPTH: usize = 128;

/// Iteratively check a value is serializable: no more than
/// [`MAX_SERIALIZE_DEPTH`] levels of nesting, and no non-finite floats (a
/// hand-built `Value::Float(NAN)` would panic the `expect`-guarded writers;
/// the parse path rejects non-finite floats, but hand-built values bypass
/// it). The check walks an explicit stack, so a hand-built value of any
/// depth is checked without recursing itself. Scalars are depth 0; every
/// array/object level adds one, matching the parse screen's counting.
fn check_serializable_depth(value: &Value) -> Result<(), OntologyError> {
    let mut stack: Vec<(&Value, usize)> = vec![(value, 0)];
    while let Some((current, depth)) = stack.pop() {
        match current {
            Value::Array(items) => {
                if depth + 1 > MAX_SERIALIZE_DEPTH {
                    return Err(OntologyError::NestingTooDeep {
                        limit: MAX_SERIALIZE_DEPTH,
                    });
                }
                stack.extend(items.iter().map(|item| (item, depth + 1)));
            }
            Value::Object(map) => {
                if depth + 1 > MAX_SERIALIZE_DEPTH {
                    return Err(OntologyError::NestingTooDeep {
                        limit: MAX_SERIALIZE_DEPTH,
                    });
                }
                stack.extend(map.values().map(|item| (item, depth + 1)));
            }
            Value::Float(f) if !f.is_finite() => {
                return Err(OntologyError::NonFiniteFloat);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Serialize a value as canonical JSON: compact, keys in document order.
/// Fails with [`OntologyError::NestingTooDeep`] past the 128-level cap
/// (hand-built values; parsed documents are pre-bounded by the parse
/// screen).
pub fn to_json_string(value: &Value) -> Result<String, OntologyError> {
    check_serializable_depth(value)?;
    let mut out = String::new();
    write_json(value, &mut out, false);
    Ok(out)
}

/// Serialize a value as canonical YAML: block style with two-space
/// indentation, keys in document order. Depth-capped like
/// [`to_json_string`] (the node builder and saphyr's emitter are
/// recursive).
pub fn to_yaml_string(value: &Value) -> Result<String, OntologyError> {
    check_serializable_depth(value)?;
    let node = yaml_node(value);
    let mut out = String::new();
    let mut emitter = YamlEmitter::new(&mut out);
    emitter.compact(false);
    emitter
        .dump(&node)
        .expect("writing YAML to a String cannot fail");
    Ok(out)
}

/// Serialize a value as canonical JSON text for free-form payload storage
/// (ADR-0226): compact, object keys sorted, so a JSON and a YAML submission
/// of the same logical document derive the same text. Depth-capped like
/// [`to_json_string`].
pub fn to_json_sorted_string(value: &Value) -> Result<String, OntologyError> {
    check_serializable_depth(value)?;
    let mut out = String::new();
    write_json(value, &mut out, true);
    Ok(out)
}

fn write_json(value: &Value, out: &mut String, sort_keys: bool) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Boolean(true) => out.push_str("true"),
        Value::Boolean(false) => out.push_str("false"),
        Value::Integer(i) => {
            // `write!` into a String is infallible.
            let _ = write!(out, "{i}");
        }
        Value::Float(f) => {
            // Finite by construction; ryu (via serde_json) emits the shortest
            // round-trip form.
            out.push_str(
                &serde_json::to_string(f).expect("a finite f64 always serializes as JSON"),
            );
        }
        Value::String(s) => {
            out.push_str(&serde_json::to_string(s).expect("a String always serializes as JSON"));
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json(item, out, sort_keys);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Entries are collected unconditionally; `sort_keys` then
            // decides only the order, so there is one emit loop.
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            out.push('{');
            for (i, (key, value)) in entries.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_entry(key, value, out, sort_keys);
            }
            out.push('}');
        }
    }
}

fn write_json_entry(key: &str, value: &Value, out: &mut String, sort_keys: bool) {
    out.push_str(&serde_json::to_string(key).expect("a &str always serializes as JSON"));
    out.push(':');
    write_json(value, out, sort_keys);
}

/// Build a borrowing saphyr node from a value, for emission by
/// [`YamlEmitter`]. Floats ride the `Representation` variant so no
/// `ordered-float` dependency is needed to name them: the plain-style
/// representation is the shortest round-trip form, which re-parses as a
/// float under the YAML 1.2 core schema.
fn yaml_node(value: &Value) -> Yaml<'_> {
    match value {
        Value::Null => Yaml::Value(Scalar::Null),
        Value::Boolean(b) => Yaml::Value(Scalar::Boolean(*b)),
        Value::Integer(i) => Yaml::Value(Scalar::Integer(*i)),
        Value::Float(f) => Yaml::Representation(format_float(*f).into(), ScalarStyle::Plain, None),
        Value::String(s) => Yaml::Value(Scalar::String(Cow::Borrowed(s.as_str()))),
        Value::Array(items) => Yaml::Sequence(items.iter().map(yaml_node).collect()),
        Value::Object(map) => {
            let mut mapping = Mapping::default();
            for (key, value) in map {
                mapping.insert(
                    Yaml::Value(Scalar::String(Cow::Borrowed(key.as_str()))),
                    yaml_node(value),
                );
            }
            Yaml::Mapping(mapping)
        }
    }
}

/// The canonical YAML/JSON float form: shortest round-trip, with a `.0`
/// suffix for integer-valued floats so the YAML core schema re-resolves
/// them as floats.
fn format_float(f: f64) -> String {
    let mut s = serde_json::to_string(&f).expect("a finite f64 always serializes as JSON");
    if f.fract() == 0.0 && !s.contains(['.', 'e', 'E']) {
        s.push_str(".0");
    }
    s
}
