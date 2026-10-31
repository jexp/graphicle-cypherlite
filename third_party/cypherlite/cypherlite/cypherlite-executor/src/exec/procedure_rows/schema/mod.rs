//! `db.schema.*` row producers: the inferred property-schema procedures
//! (`db.schema.nodeTypeProperties`, `db.schema.relTypeProperties`) live
//! here; `db.schema.visualization` and its catalog-decoration helpers live
//! in [`visualization`]. The schema is inferred by scanning stored
//! entities, never the declared graph type spec (ADR-0112).

use std::collections::{BTreeMap, BTreeSet};

use cypherlite_core::Value;

use crate::eval::value_type_name;
use crate::exec::catalog::string_list;
use crate::overlay::ExecGraph;

mod visualization;

pub(super) use visualization::schema_visualization_rows;

/// Inferred property schema for one entity category: a node's full sorted
/// label set, or a relationship's type. Built by scanning stored entities,
/// never the declared graph type spec (ADR-0112).
#[derive(Default)]
struct SchemaCategory {
    /// Number of entities seen in the category.
    entity_count: u64,
    /// Per property key: how many category entities carry it and the distinct
    /// `valueType()` names observed for its values.
    properties: BTreeMap<String, SchemaProperty>,
}

#[derive(Default)]
struct SchemaProperty {
    present_count: u64,
    type_names: BTreeSet<String>,
}

impl SchemaCategory {
    fn observe(&mut self, properties: &cypherlite_core::PropertyMap) {
        self.entity_count += 1;
        for (key, value) in properties {
            let property = self.properties.entry(key.clone()).or_default();
            property.present_count += 1;
            property.type_names.insert(value_type_name(value));
        }
    }
}

/// Rows for `db.schema.nodeTypeProperties`: nodes grouped by their full sorted
/// label set, with the inferred per-key types and mandatory flag.
pub(super) fn schema_node_type_rows(graph: &ExecGraph<'_>) -> Vec<BTreeMap<String, Value>> {
    let mut categories: BTreeMap<Vec<String>, SchemaCategory> = BTreeMap::new();
    for id in graph.all_node_ids() {
        let Some(node) = graph.node(id) else {
            continue;
        };
        let mut labels = (*node.labels).clone();
        labels.sort();
        labels.dedup();
        categories
            .entry(labels)
            .or_default()
            .observe(&node.properties);
    }
    let mut rows = Vec::new();
    for (labels, category) in categories {
        let node_type = labels
            .iter()
            .map(|label| format!(":{label}"))
            .collect::<String>();
        let fixed = BTreeMap::from([
            ("nodeType".into(), Value::String(node_type.into())),
            ("nodeLabels".into(), string_list(labels)),
        ]);
        rows.extend(category_rows(fixed, category));
    }
    rows
}

/// Rows for `db.schema.relTypeProperties`: relationships grouped by type,
/// with the inferred per-key types and mandatory flag.
pub(super) fn schema_rel_type_rows(graph: &ExecGraph<'_>) -> Vec<BTreeMap<String, Value>> {
    let mut categories: BTreeMap<String, SchemaCategory> = BTreeMap::new();
    for id in graph.all_rel_ids() {
        let Some(rel) = graph.rel(id) else {
            continue;
        };
        categories
            .entry(rel.rel_type)
            .or_default()
            .observe(&rel.properties);
    }
    let mut rows = Vec::new();
    for (rel_type, category) in categories {
        let fixed = BTreeMap::from([(
            "relType".into(),
            Value::String(format!("[:{rel_type}]").into()),
        )]);
        rows.extend(category_rows(fixed, category));
    }
    rows
}

/// Emit rows for one category: the ADR-0112 null-row rule when it has no
/// properties, else one row per property key. `fixed` carries the category
/// columns (`{nodeType, nodeLabels}` or `{relType}`).
fn category_rows(
    fixed: BTreeMap<String, Value>,
    category: SchemaCategory,
) -> Vec<BTreeMap<String, Value>> {
    // ADR-0112 null_row_rule: a property-less category still yields
    // exactly one row with `propertyName: null` — the same row shape a
    // property key's row takes, with the null name and no types.
    let entries: Vec<(Option<String>, Vec<String>, bool)> = if category.properties.is_empty() {
        vec![(None, Vec::new(), false)]
    } else {
        category
            .properties
            .into_iter()
            .map(|(name, property)| {
                (
                    Some(name),
                    property.type_names.into_iter().collect(),
                    property.present_count == category.entity_count,
                )
            })
            .collect()
    };
    entries
        .into_iter()
        .map(|(name, type_names, mandatory)| {
            fixed
                .clone()
                .into_iter()
                .chain([
                    (
                        "propertyName".into(),
                        name.map(|n| Value::String(n.into())).unwrap_or(Value::Null),
                    ),
                    ("propertyTypes".into(), string_list(type_names)),
                    ("mandatory".into(), Value::Bool(mandatory)),
                ])
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_helpers::{map, rows, string};
    use super::*;
    use crate::graph::Graph;

    #[test]
    fn node_mixed_type_property_yields_multiple_property_types() {
        let graph = Graph::default();
        graph
            .create_node(vec!["Person".into()], map(&[("age", Value::Int(1))]))
            .unwrap();
        graph
            .create_node(vec!["Person".into()], map(&[("age", string("old"))]))
            .unwrap();

        let rows = rows(&graph, "db.schema.nodeTypeProperties");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["nodeType"], string(":Person"));
        assert_eq!(rows[0]["propertyName"], string("age"));
        assert_eq!(
            rows[0]["propertyTypes"],
            Value::List(vec![string("INTEGER NOT NULL"), string("STRING NOT NULL")].into())
        );
        assert_eq!(rows[0]["mandatory"], Value::Bool(true));
    }

    #[test]
    fn node_optional_property_is_not_mandatory() {
        let graph = Graph::default();
        graph
            .create_node(
                vec!["Person".into()],
                map(&[("name", string("Ada")), ("age", Value::Int(36))]),
            )
            .unwrap();
        graph
            .create_node(vec!["Person".into()], map(&[("name", string("Grace"))]))
            .unwrap();

        let rows = rows(&graph, "db.schema.nodeTypeProperties");
        let by_name: BTreeMap<&str, Value> = rows
            .iter()
            .map(|row| {
                (
                    row["propertyName"].as_string().unwrap(),
                    row["mandatory"].clone(),
                )
            })
            .collect();
        assert_eq!(by_name["name"], Value::Bool(true));
        assert_eq!(by_name["age"], Value::Bool(false));
    }

    #[test]
    fn node_multi_label_categories_are_grouped_by_full_label_set() {
        let graph = Graph::default();
        graph
            .create_node(vec!["A".into(), "B".into()], map(&[("x", Value::Int(1))]))
            .unwrap();
        graph
            .create_node(vec!["B".into()], map(&[("y", Value::Int(2))]))
            .unwrap();

        let rows = rows(&graph, "db.schema.nodeTypeProperties");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["nodeType"], string(":A:B"));
        assert_eq!(
            rows[0]["nodeLabels"],
            Value::List(vec![string("A"), string("B")].into())
        );
        assert_eq!(rows[0]["propertyName"], string("x"));
        assert_eq!(rows[1]["nodeType"], string(":B"));
        assert_eq!(rows[1]["propertyName"], string("y"));
    }

    #[test]
    fn node_category_without_properties_yields_null_row() {
        let graph = Graph::default();
        graph.create_node(vec!["Empty".into()], map(&[])).unwrap();

        let rows = rows(&graph, "db.schema.nodeTypeProperties");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["nodeType"], string(":Empty"));
        assert_eq!(rows[0]["propertyName"], Value::Null);
        assert_eq!(rows[0]["propertyTypes"], Value::List(Vec::new().into()));
        assert_eq!(rows[0]["mandatory"], Value::Bool(false));
    }

    #[test]
    fn empty_database_yields_zero_rows() {
        let graph = Graph::default();
        assert!(rows(&graph, "db.schema.nodeTypeProperties").is_empty());
        assert!(rows(&graph, "db.schema.relTypeProperties").is_empty());
        assert!(rows(&graph, "db.schema.visualization").is_empty());
    }

    #[test]
    fn relationship_properties_are_grouped_by_type() {
        let graph = Graph::default();
        let a = graph.create_node(vec!["A".into()], map(&[])).unwrap();
        let b = graph.create_node(vec!["B".into()], map(&[])).unwrap();
        graph
            .create_rel(a, b, "KNOWS".into(), map(&[("since", Value::Int(2020))]))
            .unwrap();
        graph
            .create_rel(b, a, "KNOWS".into(), map(&[("since", Value::Int(2021))]))
            .unwrap();

        let rows = rows(&graph, "db.schema.relTypeProperties");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["relType"], string("[:KNOWS]"));
        assert_eq!(rows[0]["propertyName"], string("since"));
        assert_eq!(
            rows[0]["propertyTypes"],
            Value::List(vec![string("INTEGER NOT NULL")].into())
        );
        assert_eq!(rows[0]["mandatory"], Value::Bool(true));
    }
}
