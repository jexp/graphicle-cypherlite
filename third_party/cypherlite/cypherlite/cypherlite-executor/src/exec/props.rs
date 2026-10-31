//! Property-write validation shared by the CREATE/MERGE materialization
//! (`exec/create.rs`) and the SET/REMOVE clauses (`exec/write.rs`): one
//! typed verdict walk decides whether a value can be stored and why not
//! (ADR-0327, ADR-0339).

use std::collections::BTreeMap;

use cypherlite_core::{Error, Result, Value};

/// The `TypeError` detail for a byte array stored as a property (ADR-0327).
const BYTES_PROPERTY_DETAIL: &str = "byte array values cannot be stored as node or \
     relationship properties; they live only inside a statement (parameter to result)";

/// Why a value cannot be stored as a property.
enum PropertyInvalidity {
    /// A byte array (ADR-0327), directly or nested in a list at any depth.
    HoldsBytes,
    /// Any other non-storable shape.
    Other,
}

/// Check that `v` can be stored as a property value: `Ok`, the typed
/// `TypeError` for a byte array (or a list holding one at any depth;
/// ADR-0327), or the `InvalidPropertyType` type error for every other
/// non-storable value. Null is storable and transparent: each caller
/// applies its own null policy (drop the entry, or store it as a removal).
pub(crate) fn check_property_value(v: &Value) -> Result<()> {
    match property_value_invalidity(v) {
        None => Ok(()),
        Some(PropertyInvalidity::HoldsBytes) => Err(Error::type_error(BYTES_PROPERTY_DETAIL)),
        Some(PropertyInvalidity::Other) => Err(Error::ty("InvalidPropertyType")),
    }
}

/// The one property-value walk: `None` when `v` can be stored,
/// `Some(HoldsBytes)` when a byte array appears anywhere inside it (a list
/// nested at any depth included), `Some(Other)` for every other
/// non-storable shape. A list is storable when its non-null items are one
/// level of homogeneous scalar values; a nested list of scalars is `Other`.
fn property_value_invalidity(v: &Value) -> Option<PropertyInvalidity> {
    match v {
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::String(_) => None,
        Value::Temporal(_) | Value::Point(_) | Value::Uuid(_) => None,
        // Statement-scoped (ADR-0327): a byte array cannot be stored.
        Value::Bytes(_) => Some(PropertyInvalidity::HoldsBytes),
        // Any vector value stores; the coordinate kind does not narrow the
        // property-value vocabulary (ADR-0339).
        Value::Vector { .. } => None,
        Value::List(xs) => {
            let mut kind = None;
            for item in xs.iter() {
                if item.is_null() {
                    continue;
                }
                // A byte array anywhere inside the item is the byte-array
                // verdict; otherwise the item must be a scalar of the
                // list's one kind.
                if bytes_within(item) {
                    return Some(PropertyInvalidity::HoldsBytes);
                }
                let Some(item_kind) = list_item_kind(item) else {
                    return Some(PropertyInvalidity::Other);
                };
                if kind.is_some_and(|expected| expected != item_kind) {
                    return Some(PropertyInvalidity::Other);
                }
                kind = Some(item_kind);
            }
            None
        }
        _ => Some(PropertyInvalidity::Other),
    }
}

/// Whether a byte array appears inside `v` — the value itself or a list
/// holding one at any depth. The byte-array detail wins over the generic
/// one when a list is otherwise non-storable.
fn bytes_within(v: &Value) -> bool {
    match v {
        Value::Bytes(_) => true,
        Value::List(xs) => xs.iter().any(bytes_within),
        _ => false,
    }
}

/// The scalar kind of a storable list item; `None` for every non-scalar
/// (a nested list, map, entity or byte array).
fn list_item_kind(v: &Value) -> Option<std::mem::Discriminant<Value>> {
    match v {
        Value::Null => None,
        Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::String(_)
        | Value::Temporal(_)
        | Value::Point(_)
        | Value::Uuid(_) => Some(std::mem::discriminant(v)),
        _ => None,
    }
}

/// Drop null entries and validate every remaining value.
pub(crate) fn sanitize_props(map: BTreeMap<String, Value>) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for (k, v) in map {
        if v.is_null() {
            continue;
        }
        check_property_value(&v)?;
        out.insert(k, v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cypherlite_core::{PointValue, Temporal, VectorCoordKind};

    fn bytes() -> Value {
        Value::Bytes(vec![0, 1, 255].into())
    }

    /// ADR-0339: one walk decides validity and the reason; the byte-array
    /// detail is reported for a byte array anywhere inside the value.
    #[test]
    fn property_value_invalidity_classifies_nested_bytes() {
        let scalar = Value::Int(1);
        assert!(check_property_value(&scalar).is_ok());
        assert!(check_property_value(&Value::Null).is_ok());
        assert!(check_property_value(&Value::Temporal(Temporal::Duration {
            months: 1,
            days: 0,
            nanos: 0
        }))
        .is_ok());
        assert!(check_property_value(&Value::Point(PointValue::cartesian_2d(1.0, 2.0))).is_ok());
        assert!(check_property_value(&Value::Vector {
            coord: VectorCoordKind::Int8,
            values: vec![Value::Int(1)].into(),
        })
        .is_ok());

        // Homogeneous scalar lists store; nulls are skipped; a mixed list
        // and a nested scalar list do not.
        assert!(
            check_property_value(&Value::List(vec![Value::Int(1), Value::Int(2)].into())).is_ok()
        );
        assert!(
            check_property_value(&Value::List(vec![Value::Null, Value::Int(1)].into())).is_ok()
        );
        assert_eq!(
            check_property_value(&Value::List(
                vec![Value::Int(1), Value::String("a".into())].into()
            ))
            .unwrap_err(),
            Error::ty("InvalidPropertyType")
        );
        assert_eq!(
            check_property_value(&Value::List(
                vec![Value::List(vec![Value::Int(1)].into())].into()
            ))
            .unwrap_err(),
            Error::ty("InvalidPropertyType")
        );

        // A byte array names itself at any depth; every other non-scalar
        // keeps the generic detail.
        let bytes_detail = Error::type_error(BYTES_PROPERTY_DETAIL);
        assert_eq!(check_property_value(&bytes()).unwrap_err(), bytes_detail);
        assert_eq!(
            check_property_value(&Value::List(vec![bytes()].into())).unwrap_err(),
            bytes_detail
        );
        assert_eq!(
            check_property_value(&Value::List(
                vec![Value::List(vec![Value::Int(1), bytes()].into())].into()
            ))
            .unwrap_err(),
            bytes_detail
        );
        assert_eq!(
            check_property_value(&Value::Map(BTreeMap::new().into())).unwrap_err(),
            Error::ty("InvalidPropertyType")
        );
    }

    /// `sanitize_props` drops null entries (they are not stored) and
    /// validates every remaining value.
    #[test]
    fn sanitize_props_drops_nulls_and_validates_the_rest() {
        let map = BTreeMap::from([
            ("a".to_string(), Value::Int(1)),
            ("b".to_string(), Value::Null),
        ]);
        assert_eq!(
            sanitize_props(map).unwrap(),
            BTreeMap::from([("a".into(), Value::Int(1))])
        );
        let map = BTreeMap::from([("b".to_string(), bytes())]);
        assert_eq!(
            sanitize_props(map).unwrap_err(),
            Error::type_error(BYTES_PROPERTY_DETAIL)
        );
    }
}
