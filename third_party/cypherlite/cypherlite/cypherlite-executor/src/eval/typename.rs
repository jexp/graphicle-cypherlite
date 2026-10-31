//! Cypher value type names (`valueType()`) and their sort order.

use std::collections::HashSet;

use cypherlite_core::{temporal, Value};

pub(crate) fn value_type_name(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(_) => "BOOLEAN NOT NULL".into(),
        Value::Int(_) => "INTEGER NOT NULL".into(),
        Value::Float(_) => "FLOAT NOT NULL".into(),
        Value::String(_) => "STRING NOT NULL".into(),
        Value::Map(_) => "MAP NOT NULL".into(),
        Value::Node(_) => "NODE NOT NULL".into(),
        Value::Relationship(_) => "RELATIONSHIP NOT NULL".into(),
        Value::Path(_) => "PATH NOT NULL".into(),
        Value::Point(_) => "POINT NOT NULL".into(),
        Value::Uuid(_) => "UUID NOT NULL".into(),
        // Cypher has no byte-array type: Neo4j types a byte array as a
        // list of integers (ADR-0327), and an empty one as an empty list.
        Value::Bytes(b) if b.is_empty() => "LIST<NOTHING> NOT NULL".into(),
        Value::Bytes(_) => "LIST<INTEGER NOT NULL> NOT NULL".into(),
        Value::Temporal(t) => format!(
            "{} NOT NULL",
            match t {
                temporal::Temporal::Date(_) | temporal::Temporal::CivilDate { .. } => "DATE",
                temporal::Temporal::LocalTime(_) => "LOCAL TIME",
                temporal::Temporal::Time { .. } => "ZONED TIME",
                temporal::Temporal::LocalDateTime(_) | temporal::Temporal::CivilDateTime { .. } =>
                    "LOCAL DATETIME",
                temporal::Temporal::DateTime { .. } => "ZONED DATETIME",
                temporal::Temporal::Duration { .. } => "DURATION",
            }
        ),
        Value::Vector { coord, values } => format!(
            "VECTOR<{} NOT NULL>({}) NOT NULL",
            coord.canonical_name(),
            values.len()
        ),
        Value::List(values) => {
            if values.is_empty() {
                return "LIST<NOTHING> NOT NULL".into();
            }
            let has_null = values.iter().any(Value::is_null);
            let mut types = values
                .iter()
                .filter(|v| !v.is_null())
                .map(value_type_name)
                .map(|s| {
                    if has_null {
                        s.trim_end_matches(" NOT NULL").to_string()
                    } else {
                        s
                    }
                })
                .collect::<Vec<_>>();
            types.sort_by(|a, b| {
                let ak = value_type_sort_key(a);
                let bk = value_type_sort_key(b);
                if a.starts_with("LIST<")
                    && b.starts_with("LIST<")
                    && ak.1 == bk.1
                    && !a["LIST<".len()..].contains("LIST<")
                    && !b["LIST<".len()..].contains("LIST<")
                {
                    b.cmp(a)
                } else {
                    ak.cmp(&bk)
                }
            });
            types.dedup();
            let nested_sets = types
                .iter()
                .map(|ty| nested_list_members(ty))
                .collect::<Vec<_>>();
            let mut keep = vec![true; types.len()];
            for i in 0..types.len() {
                let Some(left) = &nested_sets[i] else {
                    continue;
                };
                for (j, right) in nested_sets.iter().enumerate() {
                    if i != j
                        && right
                            .as_ref()
                            .is_some_and(|right| left.len() < right.len() && left.is_subset(right))
                    {
                        keep[i] = false;
                        break;
                    }
                }
            }
            types = types
                .into_iter()
                .zip(keep)
                .filter_map(|(ty, keep)| keep.then_some(ty))
                .collect();
            if types.len() > 1 {
                types.retain(|ty| ty != "LIST<NOTHING> NOT NULL");
            }
            let element = if types.is_empty() {
                "NULL".into()
            } else {
                types.join(" | ")
            };
            format!("LIST<{element}> NOT NULL")
        }
    }
}

pub(crate) fn nested_list_members(name: &str) -> Option<HashSet<String>> {
    let inner = name.strip_prefix("LIST<")?.strip_suffix("> NOT NULL")?;
    if inner.contains("LIST<") {
        return None;
    }
    Some(inner.split(" | ").map(str::to_string).collect())
}

pub(crate) fn value_type_sort_key(name: &str) -> (u8, usize, &str) {
    // CIP-100 order (matching type_normalize::rank so a dynamic union
    // normalizes identically wherever it is sorted): UUID sits between
    // STRING and the numerics per the CIP-246 amendment.
    let rank = if name.starts_with("BOOLEAN") {
        0
    } else if name.starts_with("STRING") {
        1
    } else if name.starts_with("UUID") {
        2
    } else if name.starts_with("INTEGER") {
        3
    } else if name.starts_with("FLOAT") {
        4
    } else if name.starts_with("DATE ") || name == "DATE" {
        5
    } else if name.starts_with("LOCAL TIME") {
        6
    } else if name.starts_with("ZONED TIME") {
        7
    } else if name.starts_with("LOCAL DATETIME") {
        8
    } else if name.starts_with("ZONED DATETIME") {
        9
    } else if name.starts_with("DURATION") {
        10
    } else if name.starts_with("POINT") {
        11
    } else if name.starts_with("NODE") {
        12
    } else if name.starts_with("RELATIONSHIP") {
        13
    } else if name.starts_with("MAP") {
        14
    } else if name.starts_with("VECTOR") {
        15
    } else if name.starts_with("LIST") {
        16
    } else if name.starts_with("PATH") {
        17
    } else {
        18
    };
    // Union members: "A | B" counts its separators. Non-union names count
    // zero separators and yield 1 — no rank gate needed (the old
    // rank-based gate silently widened with every renumbering).
    let members = name.matches(" | ").count() + 1;
    (rank, members, name)
}
