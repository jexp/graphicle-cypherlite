//! CypherLite `Value` <-> CypherValue bridge (cypherlite-model).

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{FixedOffset, TimeZone};
use cypherlite_model::value::IsoDuration;
use cypherlite_model::wire::ColumnTypeKind;
use cypherlite_model::{CypherValue, IndexMap};

use crate::temporal::Temporal;
use crate::value::{Node, Path, Relationship, Value};

/// Classify an engine `Value` into its canonical ADR-0224 kind — the
/// engine-side spelling of `cypherlite_model::wire::column_type_kind`,
/// classifying into the same single-sourced [`ColumnTypeKind`] set.
///
/// `Value` has no `Geometry` variant (that is a `CypherValue`-only kind,
/// `Point` on this side). `Value::Uuid` has no `CypherValue` counterpart —
/// `value_to_cypher` crosses it as `Str` — so it classifies `String`: the
/// wire taxonomy cannot distinguish it, and the `Uuid` kind stays reserved
/// per ADR-0224. `Value::Bytes` stamps ADR-0224's `BINARY`: the column type
/// names the engine value, which the Bolt surface sends as PackStream Bytes
/// (ADR-0327).
pub fn column_type_kind(value: &Value) -> ColumnTypeKind {
    match value {
        Value::Null => ColumnTypeKind::Any,
        Value::Bool(_) => ColumnTypeKind::Boolean,
        Value::Int(_) => ColumnTypeKind::Integer,
        Value::Float(_) => ColumnTypeKind::Float,
        Value::String(_) | Value::Uuid(_) => ColumnTypeKind::String,
        Value::Bytes(_) => ColumnTypeKind::Binary,
        Value::List(_) => ColumnTypeKind::List,
        Value::Map(_) => ColumnTypeKind::Map,
        Value::Node(_) => ColumnTypeKind::Node,
        Value::Relationship(_) => ColumnTypeKind::Relationship,
        Value::Path(_) => ColumnTypeKind::Path,
        Value::Point(_) => ColumnTypeKind::Point,
        Value::Vector { .. } => ColumnTypeKind::Vector,
        Value::Temporal(t) => match t {
            Temporal::Date(_) | Temporal::CivilDate { .. } => ColumnTypeKind::Date,
            Temporal::LocalTime(_) | Temporal::Time { .. } => ColumnTypeKind::Time,
            Temporal::LocalDateTime(_)
            | Temporal::CivilDateTime { .. }
            | Temporal::DateTime { .. } => ColumnTypeKind::DateTime,
            Temporal::Duration { .. } => ColumnTypeKind::Duration,
        },
    }
}

/// One canonical ADR-0224 type name for an engine `Value` — the name
/// spelling of [`column_type_kind`], for the engine's
/// `QueryResult::column_types` stamp.
pub fn column_type_name(value: &Value) -> &'static str {
    column_type_kind(value).name()
}

pub fn value_to_cypher(v: &Value) -> CypherValue {
    match v {
        Value::Null => CypherValue::Null,
        Value::Bool(b) => CypherValue::Bool(*b),
        Value::Int(i) => CypherValue::Int(*i),
        Value::Float(f) => CypherValue::Float(*f),
        Value::String(s) => CypherValue::Str(String::from(&**s)),
        // CIP-246 documented deviation: the model-level CypherValue (FFI /
        // Python surface) has no UUID variant, so UUIDs cross as their
        // canonical STRING form.
        Value::Uuid(u) => CypherValue::Str(crate::value::uuid_hyphenated(*u)),
        // ADR-0327 documented deviation: CypherValue has no byte-array
        // variant, so bytes cross as the LIST<INTEGER> of their byte values
        // 0-255 — the representation ADR-0219 gives Python `bytes`
        // parameters. Only the Bolt surface carries real bytes.
        Value::Bytes(b) => {
            CypherValue::List(b.iter().map(|&x| CypherValue::Int(x.into())).collect())
        }
        Value::List(xs) => CypherValue::List(xs.iter().map(value_to_cypher).collect()),
        Value::Map(m) => CypherValue::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), value_to_cypher(v)))
                .collect(),
        ),
        Value::Node(n) => node_to_cypher(n),
        Value::Relationship(r) => rel_to_cypher(r),
        Value::Path(p) => CypherValue::Path {
            nodes: p.nodes.iter().map(node_to_cypher).collect(),
            relationships: p.rels.iter().map(rel_to_cypher).collect(),
        },
        Value::Point(p) => CypherValue::Point(p.clone()),
        Value::Vector { coord, values } => {
            let mapped: Vec<CypherValue> = values.iter().map(value_to_cypher).collect();
            CypherValue::Vector {
                coord: *coord,
                dim: mapped.len(),
                values: mapped,
            }
        }
        Value::Temporal(t) => temporal_to_cypher(t),
    }
}

/// Node -> CypherValue, shared by the [`Value::Node`] arm and the
/// [`Value::Path`] arm, which converts each path element directly instead
/// of cloning it into a [`Value`] and re-entering the generic dispatcher
/// (which cloned labels and properties a second time).
fn node_to_cypher(n: &Node) -> CypherValue {
    CypherValue::Node {
        id: n.id,
        labels: (*n.labels).clone(),
        properties: n
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), value_to_cypher(v)))
            .collect(),
    }
}

/// Relationship -> CypherValue, shared by the [`Value::Relationship`] arm
/// and the [`Value::Path`] arm (see [`node_to_cypher`]).
fn rel_to_cypher(r: &Relationship) -> CypherValue {
    CypherValue::Relationship {
        id: r.id,
        start_id: r.start,
        end_id: r.end,
        rel_type: r.rel_type.clone(),
        properties: r
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), value_to_cypher(v)))
            .collect(),
    }
}

pub fn cypher_to_value(v: &CypherValue) -> Value {
    match v {
        CypherValue::Null => Value::Null,
        CypherValue::Bool(b) => Value::Bool(*b),
        CypherValue::Int(i) => Value::Int(*i),
        CypherValue::Float(f) => Value::Float(*f),
        CypherValue::Str(s) => Value::String(s.as_str().into()),
        CypherValue::List(xs) => {
            Value::List(xs.iter().map(cypher_to_value).collect::<Vec<_>>().into())
        }
        CypherValue::Map(m) => Value::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), cypher_to_value(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
        CypherValue::Node {
            id,
            labels,
            properties,
        } => Value::Node(Arc::new(node_from_fields(*id, labels, properties))),
        CypherValue::Relationship {
            id,
            start_id,
            end_id,
            rel_type,
            properties,
        } => Value::Relationship(Arc::new(rel_from_fields(
            *id, *start_id, *end_id, rel_type, properties,
        ))),
        CypherValue::Path {
            nodes,
            relationships,
        } => {
            // `CypherValue::Path` erases its element types
            // (`Vec<CypherValue>`), so a non-entity element is
            // representable. Every producer validates the shape -- the
            // engine builds paths from graph entities and the Arrow
            // typed-JSON decoder rejects bad members -- so a mismatch here
            // is an invariant violation: fail loudly instead of silently
            // dropping the element (the old `filter_map` did).
            let nodes = nodes
                .iter()
                .map(cypher_to_node)
                .collect::<Result<Vec<_>, _>>()
                .expect("CypherValue::Path nodes must be CypherValue::Node");
            let rels = relationships
                .iter()
                .map(cypher_to_relationship)
                .collect::<Result<Vec<_>, _>>()
                .expect("CypherValue::Path relationships must be CypherValue::Relationship");
            Value::Path(Arc::new(Path { nodes, rels }))
        }
        CypherValue::Date(d) => Value::Temporal(Temporal::Date(*d)),
        CypherValue::Time(t, tz) => match tz {
            None => Value::Temporal(Temporal::LocalTime(*t)),
            Some(off) => Value::Temporal(Temporal::Time {
                time: *t,
                offset: off.local_minus_utc(),
            }),
        },
        CypherValue::LocalDateTime(dt) => Value::Temporal(Temporal::LocalDateTime(*dt)),
        CypherValue::DateTime(dt, zone) => {
            let offset = dt.offset().local_minus_utc();
            let utc = dt.naive_utc();
            Value::Temporal(Temporal::DateTime {
                utc,
                offset,
                zone: zone.clone(),
            })
        }
        CypherValue::Duration(d) => Value::Temporal(Temporal::Duration {
            months: d.months,
            days: d.days,
            nanos: d.seconds as i128 * 1_000_000_000 + d.nanoseconds as i128,
        }),
        CypherValue::Point(p) => Value::Point(p.clone()),
        CypherValue::Vector { coord, values, .. } => Value::Vector {
            coord: *coord,
            values: values
                .iter()
                .map(cypher_to_value)
                .collect::<Vec<_>>()
                .into(),
        },
        // Documented deviation (mirrors the Uuid arm in `value_to_cypher`):
        // the core `Value` has no Geometry variant, so a Geometry crossing
        // the model -> core bridge degrades to Null. Persisted properties
        // never take this path: the storage codec rejects Geometry before
        // conversion (`reject_geometry` in cypherlite-storage).
        CypherValue::Geometry(_) => Value::Null,
    }
}

/// Node construction from `CypherValue::Node` fields: infallible because
/// the caller's pattern already proved the variant.
fn node_from_fields(
    id: i64,
    labels: &[String],
    properties: &IndexMap<String, CypherValue>,
) -> Node {
    Node {
        id,
        labels: Arc::new(labels.to_vec()),
        properties: crate::PropertyMap::share(
            properties
                .iter()
                .map(|(k, v)| (k.clone(), cypher_to_value(v)))
                .collect::<std::collections::BTreeMap<_, _>>(),
        ),
    }
}

/// Typed conversion of a `CypherValue::Path` node element: errors on any
/// other variant, so a malformed path element cannot be silently dropped.
fn cypher_to_node(v: &CypherValue) -> Result<Node, String> {
    match v {
        CypherValue::Node {
            id,
            labels,
            properties,
        } => Ok(node_from_fields(*id, labels, properties)),
        other => Err(format!(
            "path node element must be NODE, got {}",
            other.type_name()
        )),
    }
}

/// Relationship construction from `CypherValue::Relationship` fields (see
/// [`node_from_fields`]).
fn rel_from_fields(
    id: i64,
    start: i64,
    end: i64,
    rel_type: &str,
    properties: &IndexMap<String, CypherValue>,
) -> Relationship {
    Relationship {
        id,
        start,
        end,
        rel_type: rel_type.to_string(),
        properties: crate::PropertyMap::share(
            properties
                .iter()
                .map(|(k, v)| (k.clone(), cypher_to_value(v)))
                .collect::<std::collections::BTreeMap<_, _>>(),
        ),
    }
}

/// Typed conversion of a `CypherValue::Path` relationship element: errors
/// on any other variant (see [`cypher_to_node`]).
fn cypher_to_relationship(v: &CypherValue) -> Result<Relationship, String> {
    match v {
        CypherValue::Relationship {
            id,
            start_id,
            end_id,
            rel_type,
            properties,
        } => Ok(rel_from_fields(
            *id, *start_id, *end_id, rel_type, properties,
        )),
        other => Err(format!(
            "path relationship element must be RELATIONSHIP, got {}",
            other.type_name()
        )),
    }
}

fn temporal_to_cypher(t: &Temporal) -> CypherValue {
    match t {
        Temporal::Date(d) => CypherValue::Date(*d),
        Temporal::CivilDate { year, month, day } => NaiveDateFallback::date(*year, *month, *day),
        Temporal::LocalTime(tm) => CypherValue::Time(*tm, None),
        Temporal::Time { time, offset } => CypherValue::Time(*time, FixedOffset::east_opt(*offset)),
        Temporal::LocalDateTime(dt) => CypherValue::LocalDateTime(*dt),
        Temporal::CivilDateTime {
            year,
            month,
            day,
            time,
        } => match chrono::NaiveDate::from_ymd_opt(*year as i32, *month, *day) {
            Some(d) => CypherValue::LocalDateTime(d.and_time(*time)),
            None => CypherValue::Str(t.iso_string()),
        },
        Temporal::DateTime { utc, offset, zone } => {
            let Some(off) = FixedOffset::east_opt(*offset) else {
                return CypherValue::Str(t.iso_string());
            };
            let dt = off.from_utc_datetime(utc);
            CypherValue::DateTime(dt, zone.clone())
        }
        Temporal::Duration {
            months,
            days,
            nanos,
        } => {
            let seconds = (nanos / 1_000_000_000) as i64;
            let nanoseconds = (nanos % 1_000_000_000) as i64;
            CypherValue::Duration(IsoDuration::new(*months, *days, seconds, nanoseconds))
        }
    }
}

struct NaiveDateFallback;

impl NaiveDateFallback {
    fn date(year: i64, month: u32, day: u32) -> CypherValue {
        match chrono::NaiveDate::from_ymd_opt(year as i32, month, day) {
            Some(d) => CypherValue::Date(d),
            None => CypherValue::Str(format!("{year:04}-{month:02}-{day:02}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: i64) -> Node {
        Node {
            id,
            labels: Arc::new(vec!["L".into()]),
            properties: crate::PropertyMap::share(BTreeMap::from([(
                "name".to_string(),
                Value::String("x".into()),
            )])),
        }
    }

    fn rel(id: i64, start: i64, end: i64) -> Relationship {
        Relationship {
            id,
            start,
            end,
            rel_type: "R".into(),
            properties: crate::PropertyMap::share(BTreeMap::from([(
                "w".to_string(),
                Value::Float(0.5),
            )])),
        }
    }

    // The Path arm converts each element directly through the extracted
    // helpers: same output as the generic dispatcher, with no
    // clone-wrap-redispatch.
    #[test]
    fn value_to_cypher_path_matches_direct_conversion() {
        let path = Value::from_path(Path {
            nodes: vec![node(1), node(2)],
            rels: vec![rel(3, 1, 2)],
        });
        let CypherValue::Path {
            nodes,
            relationships,
        } = value_to_cypher(&path)
        else {
            panic!("path did not convert to CypherValue::Path");
        };
        assert_eq!(
            nodes,
            vec![
                value_to_cypher(&Value::from_node(node(1))),
                value_to_cypher(&Value::from_node(node(2))),
            ]
        );
        assert_eq!(
            relationships,
            vec![value_to_cypher(&Value::from_relationship(rel(3, 1, 2)))]
        );
    }

    // A well-formed CypherValue::Path round-trips, and the typed element
    // helpers error on a mismatched element instead of silently dropping
    // it (the old `filter_map` behavior).
    #[test]
    fn cypher_to_value_path_roundtrip_and_typed_element_errors() {
        let path = Value::from_path(Path {
            nodes: vec![node(1), node(2)],
            rels: vec![rel(3, 1, 2)],
        });
        let cypher = value_to_cypher(&path);
        assert_eq!(cypher_to_value(&cypher), path);

        let bad = cypher_to_node(&CypherValue::Int(1)).expect_err("non-node accepted");
        assert!(bad.contains("INTEGER"), "unexpected error: {bad}");
        let bad = cypher_to_relationship(&cypher).expect_err("path accepted as relationship");
        assert!(bad.contains("PATH"), "unexpected error: {bad}");
    }
}
