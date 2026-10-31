//! `IS TYPED` / property-value type checks against `TypeExpr`.

use cypherlite_core::{temporal, Value};
use cypherlite_ir::{PredefinedType, TypeExpr};

pub(crate) fn value_matches_type(value: &Value, ty: &TypeExpr) -> bool {
    match ty {
        TypeExpr::Predefined { kind, not_null } => {
            if value.is_null() {
                return !not_null && *kind != PredefinedType::Nothing;
            }
            match kind {
                PredefinedType::Nothing | PredefinedType::Null => false,
                PredefinedType::Any => true,
                PredefinedType::Boolean => matches!(value, Value::Bool(_)),
                PredefinedType::String => matches!(value, Value::String(_)),
                PredefinedType::Uuid => matches!(value, Value::Uuid(_)),
                PredefinedType::Integer => matches!(value, Value::Int(_)),
                PredefinedType::Float => matches!(value, Value::Float(_)),
                PredefinedType::Date => matches!(
                    value,
                    Value::Temporal(
                        temporal::Temporal::Date(_) | temporal::Temporal::CivilDate { .. }
                    )
                ),
                PredefinedType::Duration => {
                    matches!(value, Value::Temporal(temporal::Temporal::Duration { .. }))
                }
                PredefinedType::Point => matches!(value, Value::Point(_)),
                PredefinedType::LocalTime => {
                    matches!(value, Value::Temporal(temporal::Temporal::LocalTime(_)))
                }
                PredefinedType::ZonedTime => {
                    matches!(value, Value::Temporal(temporal::Temporal::Time { .. }))
                }
                PredefinedType::LocalDateTime => matches!(
                    value,
                    Value::Temporal(
                        temporal::Temporal::LocalDateTime(_)
                            | temporal::Temporal::CivilDateTime { .. }
                    )
                ),
                PredefinedType::ZonedDateTime => {
                    matches!(value, Value::Temporal(temporal::Temporal::DateTime { .. }))
                }
                PredefinedType::Node => matches!(value, Value::Node(_)),
                PredefinedType::Relationship => matches!(value, Value::Relationship(_)),
                PredefinedType::Path => matches!(value, Value::Path(_)),
                PredefinedType::Map => matches!(value, Value::Map(_)),
            }
        }
        TypeExpr::List { element, not_null } => {
            if value.is_null() {
                !not_null
            } else {
                match value {
                    Value::List(xs) => xs.iter().all(|x| value_matches_type(x, element)),
                    // A byte array is a LIST<INTEGER NOT NULL> (ADR-0327).
                    Value::Bytes(b) => b
                        .iter()
                        .all(|&x| value_matches_type(&Value::Int(x.into()), element)),
                    _ => false,
                }
            }
        }
        TypeExpr::PropertyValue { not_null } => {
            if value.is_null() {
                !not_null
            } else {
                is_property_value(value)
            }
        }
        TypeExpr::Union(items) => items.iter().any(|t| value_matches_type(value, t)),
        TypeExpr::Vector {
            coord,
            dimension,
            not_null,
        } => {
            if value.is_null() {
                return !not_null;
            }
            matches!(
                value,
                Value::Vector { coord: actual, values }
                    if coord.as_ref().is_none_or(|c| c == actual.canonical_name())
                        && dimension.is_none_or(|d| d as usize == values.len())
            )
        }
    }
}

pub(crate) fn is_property_value(value: &Value) -> bool {
    fn scalar(v: &Value) -> bool {
        matches!(
            v,
            Value::Bool(_)
                | Value::Int(_)
                | Value::Float(_)
                | Value::String(_)
                | Value::Temporal(_)
                | Value::Point(_)
        )
    }
    scalar(value)
        || matches!(value, Value::Vector { .. })
        || matches!(value, Value::List(xs) if xs.iter().all(scalar))
}
