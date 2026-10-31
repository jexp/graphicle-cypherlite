//! The ordered JSON-equivalent value model of ADR-0227 §4.
//!
//! Both accepted syntaxes (JSON, YAML 1.2) are parsed into [`Value`]: maps
//! with string keys in source order, lists, strings, integers, floats,
//! booleans and null. Source key order is preserved — ADR-0226's
//! deterministic id assignment and ADR-0227 §3's canonical serialization
//! depend on it.

use std::fmt;

use indexmap::IndexMap;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;

/// An ordered JSON-equivalent value (ADR-0227 §4).
///
/// Object keys are strings kept in source order via [`IndexMap`]. Integers
/// and floats are distinct variants: `3` parses to [`Value::Integer`], `3.0`
/// to [`Value::Float`], which the schema's `type: integer` checks rely on.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `null` / `~`.
    Null,
    /// A boolean.
    Boolean(bool),
    /// An integer (`i64`).
    Integer(i64),
    /// A finite float (`f64`). Non-finite floats are rejected at parse time.
    Float(f64),
    /// A string.
    String(String),
    /// A list, in source order.
    Array(Vec<Value>),
    /// A mapping with string keys in source order.
    Object(IndexMap<String, Value>),
}

impl Value {
    /// The object map, if this value is an object.
    #[must_use]
    pub fn as_object(&self) -> Option<&IndexMap<String, Value>> {
        match self {
            Value::Object(map) => Some(map),
            _ => None,
        }
    }

    /// The list elements, if this value is an array.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// The string, if this value is a string.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    /// The integer, if this value is an integer.
    #[must_use]
    pub fn as_integer(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }

    /// The boolean, if this value is a boolean.
    #[must_use]
    pub fn as_boolean(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// Whether this value is null.
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// Serde deserialization used by the strict-JSON parse path
/// ([`crate::parse`]). Hand-written (no derive, no `serde_json::Value`
/// round-trip) so that object key order is preserved without serde_json's
/// `preserve_order` feature.
impl<'de> serde::Deserialize<'de> for Value {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(ValueVisitor)
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Boolean(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(Value::Integer(v))
    }

    fn visit_u64<E>(self, v: u64) -> Result<Value, E>
    where
        E: de::Error,
    {
        i64::try_from(v)
            .map(Value::Integer)
            .map_err(|_| E::custom(format!("integer {v} is out of the supported i64 range")))
    }

    fn visit_f64<E>(self, v: f64) -> Result<Value, E>
    where
        E: de::Error,
    {
        if v.is_finite() {
            Ok(Value::Float(v))
        } else {
            Err(E::custom("non-finite floats are not supported"))
        }
    }

    fn visit_str<E>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Value::deserialize(deserializer)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A>(self, mut access: A) -> Result<Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut map = IndexMap::with_capacity(access.size_hint().unwrap_or(0));
        while let Some((key, value)) = access.next_entry::<String, Value>()? {
            // Check-then-move: the key is cloned nowhere on the success
            // path; the duplicate error borrows it before the insert moves
            // it in.
            if map.contains_key(&key) {
                return Err(de::Error::custom(format!("duplicate mapping key '{key}'")));
            }
            map.insert(key, value);
        }
        Ok(Value::Object(map))
    }
}
