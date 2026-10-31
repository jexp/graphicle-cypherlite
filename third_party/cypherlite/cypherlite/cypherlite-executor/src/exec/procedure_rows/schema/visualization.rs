//! `db.schema.visualization` row producer (LABS-150) and its per-label
//! catalog-decoration helpers (ADR-0166). The graph shape is inferred by
//! scanning stored entities, never the declared graph type spec
//! (ADR-0112); the `indexes`/`constraints` decoration is read from the
//! catalog at call time.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;

use cypherlite_catalog::{ConstraintDef, IndexDef};
use cypherlite_core::{value_to_cypher, Node, Relationship, Value};
use cypherlite_model::ConstraintType;

use crate::exec::catalog::string_list;
use crate::overlay::ExecGraph;
use crate::redo::split_legacy_property_encoding;

/// Rows for `db.schema.visualization` (LABS-150): one row whose `nodes`
/// list carries one virtual node per stored label and whose
/// `relationships` list carries one virtual relationship per distinct
/// (startLabel, relType, endLabel) triple, expanding multi-label endpoints
/// over every label combination (Neo4j's shape). Virtual entities carry
/// negative ids allocated from -1 down and their nodes are registered via
/// the graph layer's virtual-node side table
/// (`overlay::register_virtual_node`),
/// so `startNode()`/`endNode()` resolve them without a store lookup and
/// writes against them follow the CIP-257 virtual-entity rules. Each
/// virtual relationship's property map is exactly `{name: <relType>}`
/// (ADR-0187), matching Neo4j; `r.name` reads back the relationship's type.
/// Both lists are sorted (labels alphabetical; triples by startLabel,
/// relType, endLabel). The empty graph yields zero rows. Each virtual
/// node's `indexes`/`constraints` lists are populated from the catalog at
/// call time (ADR-0166).
pub(crate) fn schema_visualization_rows(graph: &ExecGraph<'_>) -> Vec<BTreeMap<String, Value>> {
    let mut labels = BTreeSet::new();
    for id in graph.all_node_ids() {
        labels.extend(graph.node_labels(id).unwrap_or_default());
    }
    if labels.is_empty() {
        return Vec::new();
    }

    // One virtual node per label. Neo4j's virtual nodes carry
    // `{name, indexes, constraints}`: `name` is the honest label and
    // `indexes`/`constraints` are read from the catalog at call time via
    // the same overlay-aware accessors SHOW INDEXES/SHOW CONSTRAINTS
    // render from (ADR-0166), so in-transaction DDL is visible here too.
    // Relationship constraints render on no virtual node (Neo4j parity).
    let catalog_indexes = graph.named_indexes();
    let catalog_constraints = graph.named_constraints();
    let mut counter: i64 = -1;
    let mut label_node_id: BTreeMap<String, i64> = BTreeMap::new();
    let mut nodes: Vec<Value> = Vec::with_capacity(labels.len());
    for label in &labels {
        let id = counter;
        counter -= 1;
        label_node_id.insert(label.clone(), id);

        let (indexes, constraints) =
            label_catalog_details(label, &catalog_indexes, &catalog_constraints);
        let node = Value::from_node(Node {
            id,
            labels: Arc::new(vec![label.clone()]),
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([
                ("name".into(), Value::String(label.as_str().into())),
                ("indexes".into(), string_list(indexes)),
                ("constraints".into(), string_list(constraints)),
            ])),
        });
        crate::overlay::register_virtual_node(id, value_to_cypher(&node));
        nodes.push(node);
    }

    // One virtual relationship per (startLabel, relType, endLabel) triple —
    // where the triple set is the per-type CARTESIAN PRODUCT of every label
    // starting a rel of the type and every label ending one, not just the
    // pairs present in data (LABS-257 follow-up; verified empirically
    // against the 2026.09 reference: a chain `(:A)-[:NEXT]->(:B)-[:NEXT]->(:C)`
    // yields A→B, B→B, B→C, and A→C, and contact-tracing's PART_OF chain
    // over {Place, Region, Country} × {Region, Country, Continent} yields
    // all 9). A multi-label endpoint contributes every label of its node.
    // Each rel carries exactly `{name: <relType>}` (ADR-0187).
    let mut start_labels: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut end_labels: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for id in graph.all_rel_ids() {
        let Some(rel_type) = graph.rel_type(id) else {
            continue;
        };
        let Some((start, end)) = graph.rel_ends(id) else {
            continue;
        };
        for label in graph.node_labels(start).unwrap_or_default() {
            start_labels
                .entry(rel_type.clone())
                .or_default()
                .insert(label);
        }
        for label in graph.node_labels(end).unwrap_or_default() {
            end_labels
                .entry(rel_type.clone())
                .or_default()
                .insert(label);
        }
    }
    let mut triples: BTreeSet<(String, String, String)> = BTreeSet::new();
    for (rel_type, starts) in &start_labels {
        for start_label in starts {
            for end_label in end_labels
                .get(rel_type)
                .into_iter()
                .flat_map(|ends| ends.iter())
            {
                triples.insert((start_label.clone(), rel_type.clone(), end_label.clone()));
            }
        }
    }
    let mut relationships: Vec<Value> = Vec::with_capacity(triples.len());
    for (start_label, rel_type, end_label) in triples {
        // By construction both lookups hit: triple labels were collected
        // from `labels`, which populated `label_node_id`.
        let start = label_node_id[&start_label];
        let end = label_node_id[&end_label];
        let id = counter;
        counter -= 1;
        // ADR-0187: the virtual relationship's property map is exactly
        // `{name: <relType>}`, the type read back as `r.name` (Neo4j
        // parity). Build the value before `rel_type` moves into the struct.
        let name = Value::String(rel_type.as_str().into());
        relationships.push(Value::from_relationship(Relationship {
            id,
            start,
            end,
            rel_type,
            properties: cypherlite_core::PropertyMap::share(BTreeMap::from([(
                "name".into(),
                name,
            )])),
        }));
    }

    vec![BTreeMap::from([
        ("nodes".into(), Value::List(nodes.into())),
        ("relationships".into(), Value::List(relationships.into())),
    ])]
}

/// Per-label catalog decoration for one `db.schema.visualization()` virtual
/// node (ADR-0166): the sorted decoded property of every catalog index on
/// `label` (constraint-backing indexes excluded), and the sorted rendered
/// `Constraint(...)` string of every node constraint on `label`.
fn label_catalog_details(
    label: &str,
    indexes: &[IndexDef],
    constraints: &[ConstraintDef],
) -> (Vec<String>, Vec<String>) {
    // A unique/key constraint's backing index (installed under the
    // constraint's own name by `catalog_create_constraint`) is excluded from
    // `indexes`: Neo4j surfaces it only as `ownedIndex` on the constraint
    // string, a field cypherlite omits (ADR-0166); SHOW INDEXES intentionally
    // still lists it (Neo4j parity there).
    let constraint_backing: BTreeSet<&str> = constraints
        .iter()
        .filter(|constraint| constraint.is_index_backed())
        .map(|constraint| constraint.name.as_str())
        .collect();
    // One string per catalog index on this label (constraint-backing
    // indexes excluded): the index's property with the legacy
    // `property#kind` encoding decoded off, sorted ascending (ADR-0166).
    let mut index_properties: Vec<String> = indexes
        .iter()
        .filter(|index| index.label == label && !constraint_backing.contains(index.name.as_str()))
        .map(|index| {
            split_legacy_property_encoding(&index.property)
                .0
                .to_string()
        })
        .collect();
    index_properties.sort_unstable();
    // Node constraints on this label, sorted by constraint name.
    let mut label_constraints: Vec<&ConstraintDef> = constraints
        .iter()
        .filter(|constraint| !constraint.relationship && constraint.label == label)
        .collect();
    label_constraints.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    let rendered = label_constraints
        .into_iter()
        .filter_map(visualization_constraint_string)
        .collect();
    (index_properties, rendered)
}

/// Render one node constraint the way Neo4j's `db.schema.visualization()`
/// renders it: `Constraint( name='<name>', type='<TYPE>', schema=(:<Label>
/// {<props>}) )`, with the type in Neo4j's space style (not the underscore
/// style `SHOW CONSTRAINTS` uses) and `<props>` the comma-joined property
/// list (a property-less constraint renders `schema=(:<Label>)`). No
/// `id`/`ownedIndex` fields: cypherlite's catalog is name-keyed and has no
/// schema-rule or owned-index ids to report (ADR-0166).
fn visualization_constraint_string(constraint: &ConstraintDef) -> Option<String> {
    // Fail closed at the parse boundary: an unknown kind tag skips the
    // constraint rather than rendering the raw tag.
    let kind = ConstraintType::from_str(&constraint.kind).ok()?;
    let schema = if constraint.properties.is_empty() {
        format!("(:{})", constraint.label)
    } else {
        format!(
            "(:{} {{{}}})",
            constraint.label,
            constraint.properties.join(",")
        )
    };
    Some(format!(
        "Constraint( name='{}', type='{}', schema={} )",
        constraint.name,
        neo4j_space_constraint_type_name(kind),
        schema
    ))
}

/// Neo4j's space-style name for a constraint kind — the style
/// `db.schema.visualization()`'s `Constraint(...)` string uses, as opposed
/// to the underscore style `SHOW CONSTRAINTS` uses (ADR-0166). Lives in the
/// executor (not on `ConstraintType`) because it is a Neo4j rendering
/// concern, like the underscore mapping in `catalog_show`; the model crate
/// holds the data-model tags. The four relationship label kinds never reach
/// this rendering (relationship constraints appear on no virtual node);
/// they map to their space-style names only to keep the match exhaustive.
fn neo4j_space_constraint_type_name(kind: ConstraintType) -> &'static str {
    match kind {
        ConstraintType::Unique => "NODE PROPERTY UNIQUENESS",
        ConstraintType::Key => "NODE KEY",
        ConstraintType::Exists => "NODE PROPERTY EXISTENCE",
        ConstraintType::PropertyType => "NODE PROPERTY TYPE",
        ConstraintType::LabelExistence | ConstraintType::NodeLabelExistence => {
            "NODE LABEL EXISTENCE"
        }
        ConstraintType::SourceLabel => "RELATIONSHIP SOURCE LABEL",
        ConstraintType::SourceLabelExistence => "RELATIONSHIP SOURCE LABEL EXISTENCE",
        ConstraintType::TargetLabel => "RELATIONSHIP TARGET LABEL",
        ConstraintType::TargetLabelExistence => "RELATIONSHIP TARGET LABEL EXISTENCE",
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::test_helpers::{map, rows, string};
    use super::*;
    use crate::graph::Graph;

    #[test]
    fn schema_visualization_builds_one_virtual_node_per_label_and_rel_per_triple() {
        // LABS-150: multi-label, multi-rel-type graph — one virtual node per
        // label (sorted), one virtual relationship per distinct
        // (startLabel, relType, endLabel) triple with multi-label endpoints
        // expanded over every combination; all virtual ids negative.
        let graph = Graph::default();
        let alice = graph
            .create_node(vec!["Person".into()], map(&[("name", string("Alice"))]))
            .unwrap();
        let carol = graph
            .create_node(
                vec!["Employee".into(), "Person".into()],
                map(&[("name", string("Carol"))]),
            )
            .unwrap();
        let movie = graph
            .create_node(vec!["Movie".into()], map(&[("title", string("Dune"))]))
            .unwrap();
        graph
            .create_rel(alice, movie, "ACTED_IN".into(), map(&[]))
            .unwrap();
        graph
            .create_rel(carol, movie, "ACTED_IN".into(), map(&[]))
            .unwrap();
        graph
            .create_rel(alice, carol, "KNOWS".into(), map(&[]))
            .unwrap();

        let rows = rows(&graph, "db.schema.visualization");
        assert_eq!(rows.len(), 1);
        let Value::List(nodes) = &rows[0]["nodes"] else {
            panic!("nodes is a list, got {:?}", rows[0]["nodes"]);
        };
        let Value::List(rels) = &rows[0]["relationships"] else {
            panic!(
                "relationships is a list, got {:?}",
                rows[0]["relationships"]
            );
        };

        // Labels sorted: Employee, Movie, Person — ids -1, -2, -3.
        let mut node_ids = BTreeMap::new();
        for (index, expected_label) in ["Employee", "Movie", "Person"].iter().enumerate() {
            let Value::Node(node) = &nodes[index] else {
                panic!("virtual node, got {:?}", nodes[index]);
            };
            assert_eq!(
                node.labels.as_slice(),
                [expected_label.to_string()].as_slice()
            );
            assert_eq!(node.id, -(index as i64) - 1);
            assert_eq!(
                node.properties["name"],
                Value::String((*expected_label).into())
            );
            // ADR-0166: with an empty catalog the populated lists stay empty.
            assert_eq!(node.properties["indexes"], Value::List(Vec::new().into()));
            assert_eq!(
                node.properties["constraints"],
                Value::List(Vec::new().into())
            );
            node_ids.insert(expected_label.to_string(), node.id);
        }

        // Triples sorted by (startLabel, relType, endLabel):
        // Employee-ACTED_IN->Movie, Person-ACTED_IN->Movie (Carol expands to
        // both), Person-KNOWS->Employee, Person-KNOWS->Person.
        let expected: &[(&str, &str, &str)] = &[
            ("Employee", "ACTED_IN", "Movie"),
            ("Person", "ACTED_IN", "Movie"),
            ("Person", "KNOWS", "Employee"),
            ("Person", "KNOWS", "Person"),
        ];
        assert_eq!(rels.len(), expected.len());
        for (index, (start_label, rel_type, end_label)) in expected.iter().enumerate() {
            let Value::Relationship(rel) = &rels[index] else {
                panic!("virtual relationship, got {:?}", rels[index]);
            };
            assert_eq!(rel.rel_type, *rel_type);
            assert_eq!(rel.start, node_ids[*start_label]);
            assert_eq!(rel.end, node_ids[*end_label]);
            assert!(rel.id < 0, "virtual relationship id, got {}", rel.id);
            // ADR-0187: the property map is exactly `{name: <relType>}`.
            assert_eq!(
                rel.properties.as_ref(),
                &BTreeMap::from([("name".into(), string(rel_type))])
            );
            // Registered for startNode()/endNode() fallback resolution:
            // resolve the endpoint through the same graph-level lookup the
            // entity builtins use (`ExecGraph::node_value`).
            let exec_graph = ExecGraph::read(&graph);
            let Value::Node(start_node) = exec_graph.node_value(rel.start) else {
                panic!("start node {} resolves via node_value", rel.start);
            };
            assert_eq!(start_node.id, rel.start);
        }
    }

    #[test]
    fn schema_visualization_populates_indexes_and_constraints_from_catalog() {
        // LABS-172 / ADR-0166: the virtual label node's `indexes` holds the
        // decoded property of every non-constraint-backing catalog index on
        // the label (sorted), and `constraints` holds the pinned
        // `Constraint(...)` rendering of every node constraint on the label
        // (sorted by name, Neo4j space-style type, no id/ownedIndex fields).
        let graph = Graph::default();
        graph
            .create_node(
                vec!["Client".into()],
                map(&[("id", Value::Int(1)), ("name", string("Ada"))]),
            )
            .unwrap();
        graph
            .install_named_constraint(ConstraintDef {
                name: "client_id_unique".into(),
                relationship: false,
                label: "Client".into(),
                properties: vec!["id".into()],
                kind: "unique".into(),
                property_type: None,
            })
            .unwrap();
        // The backing index CREATE CONSTRAINT installs for
        // `client_id_unique` (named after the constraint); it must not
        // appear in `indexes`.
        graph
            .install_named_index(IndexDef {
                name: "client_id_unique".into(),
                label: "Client".into(),
                property: "id#RANGE#client_id_unique".into(),
            })
            .unwrap();
        graph
            .install_named_constraint(ConstraintDef {
                name: "client_name_exists".into(),
                relationship: false,
                label: "Client".into(),
                properties: vec!["name".into()],
                kind: "exists".into(),
                property_type: None,
            })
            .unwrap();
        graph
            .install_named_index(IndexDef {
                name: "client_name_index".into(),
                label: "Client".into(),
                // The legacy `property#kind#name` encoding CREATE INDEX
                // writes; the rendering decodes it to the bare property.
                property: "name#RANGE#client_name_index".into(),
            })
            .unwrap();

        let rows = rows(&graph, "db.schema.visualization");
        assert_eq!(rows.len(), 1);
        let Value::List(nodes) = &rows[0]["nodes"] else {
            panic!("nodes is a list, got {:?}", rows[0]["nodes"]);
        };
        assert_eq!(nodes.len(), 1);
        let Value::Node(node) = &nodes[0] else {
            panic!("virtual node, got {:?}", nodes[0]);
        };
        assert_eq!(node.labels.as_slice(), ["Client".to_string()].as_slice());
        assert_eq!(
            node.properties["indexes"],
            Value::List(vec![string("name")].into())
        );
        assert_eq!(
            node.properties["constraints"],
            Value::List(
                vec![
                    string(
                        "Constraint( name='client_id_unique', \
                     type='NODE PROPERTY UNIQUENESS', schema=(:Client {id}) )"
                    ),
                    string(
                        "Constraint( name='client_name_exists', \
                     type='NODE PROPERTY EXISTENCE', schema=(:Client {name}) )"
                    ),
                ]
                .into()
            )
        );
    }

    #[test]
    fn schema_visualization_unique_constraint_only_yields_empty_indexes() {
        // ADR-0166: a label carrying ONLY a unique constraint (no explicit
        // index) renders `indexes = []` — the constraint's backing index is
        // excluded (Neo4j lists it only as `ownedIndex` on the constraint
        // string, a field cypherlite omits) — while `constraints` carries
        // the rendered constraint string.
        let graph = Graph::default();
        graph
            .create_node(vec!["Client".into()], map(&[("id", Value::Int(1))]))
            .unwrap();
        graph
            .install_named_constraint(ConstraintDef {
                name: "client_id_unique".into(),
                relationship: false,
                label: "Client".into(),
                properties: vec!["id".into()],
                kind: "unique".into(),
                property_type: None,
            })
            .unwrap();
        // The backing index CREATE CONSTRAINT installs alongside, named
        // after the constraint with the legacy `property#kind#name`
        // encoding.
        graph
            .install_named_index(IndexDef {
                name: "client_id_unique".into(),
                label: "Client".into(),
                property: "id#RANGE#client_id_unique".into(),
            })
            .unwrap();

        let rows = rows(&graph, "db.schema.visualization");
        assert_eq!(rows.len(), 1);
        let Value::List(nodes) = &rows[0]["nodes"] else {
            panic!("nodes is a list, got {:?}", rows[0]["nodes"]);
        };
        assert_eq!(nodes.len(), 1);
        let Value::Node(node) = &nodes[0] else {
            panic!("virtual node, got {:?}", nodes[0]);
        };
        assert_eq!(node.labels.as_slice(), ["Client".to_string()].as_slice());
        assert_eq!(node.properties["indexes"], Value::List(Vec::new().into()));
        assert_eq!(
            node.properties["constraints"],
            Value::List(
                vec![string(
                    "Constraint( name='client_id_unique', \
                 type='NODE PROPERTY UNIQUENESS', schema=(:Client {id}) )"
                )]
                .into()
            )
        );
    }

    #[test]
    fn schema_visualization_constraint_string_interpolates_special_characters_raw() {
        // Security pin (Neo4j parity): a constraint name or label containing
        // a quote or brace is interpolated RAW into the `Constraint(...)`
        // string — no escaping. Guards against future "helpful" escaping
        // diverging from the pinned ADR-0166 rendering.
        let graph = Graph::default();
        graph
            .create_node(vec!["La}bel".into()], map(&[("id", Value::Int(1))]))
            .unwrap();
        graph
            .install_named_constraint(ConstraintDef {
                name: "weird'} {name".into(),
                relationship: false,
                label: "La}bel".into(),
                properties: vec!["id".into()],
                kind: "unique".into(),
                property_type: None,
            })
            .unwrap();

        let rows = rows(&graph, "db.schema.visualization");
        assert_eq!(rows.len(), 1);
        let Value::List(nodes) = &rows[0]["nodes"] else {
            panic!("nodes is a list, got {:?}", rows[0]["nodes"]);
        };
        assert_eq!(nodes.len(), 1);
        let Value::Node(node) = &nodes[0] else {
            panic!("virtual node, got {:?}", nodes[0]);
        };
        assert_eq!(
            node.properties["constraints"],
            Value::List(
                vec![string(
                    "Constraint( name='weird'} {name', \
                 type='NODE PROPERTY UNIQUENESS', schema=(:La}bel {id}) )"
                )]
                .into()
            )
        );
    }

    #[test]
    fn schema_visualization_relationship_constraint_appears_on_no_virtual_node() {
        // ADR-0166 / Neo4j parity: a relationship constraint is skipped
        // entirely — it renders on no virtual node, and the virtual
        // relationship carries only its ADR-0187 `name` property.
        let graph = Graph::default();
        let a = graph.create_node(vec!["Client".into()], map(&[])).unwrap();
        let b = graph.create_node(vec!["Client".into()], map(&[])).unwrap();
        graph
            .create_rel(
                a,
                b,
                "TRANSFER_TO".into(),
                map(&[("amount", Value::Int(10))]),
            )
            .unwrap();
        graph
            .install_named_constraint(ConstraintDef {
                name: "txn_amount_exists".into(),
                relationship: true,
                label: "TRANSFER_TO".into(),
                properties: vec!["amount".into()],
                kind: "exists".into(),
                property_type: None,
            })
            .unwrap();

        let rows = rows(&graph, "db.schema.visualization");
        assert_eq!(rows.len(), 1);
        let Value::List(nodes) = &rows[0]["nodes"] else {
            panic!("nodes is a list, got {:?}", rows[0]["nodes"]);
        };
        assert_eq!(nodes.len(), 1);
        let Value::Node(node) = &nodes[0] else {
            panic!("virtual node, got {:?}", nodes[0]);
        };
        assert_eq!(node.properties["indexes"], Value::List(Vec::new().into()));
        assert_eq!(
            node.properties["constraints"],
            Value::List(Vec::new().into())
        );
        let Value::List(rels) = &rows[0]["relationships"] else {
            panic!(
                "relationships is a list, got {:?}",
                rows[0]["relationships"]
            );
        };
        assert_eq!(rels.len(), 1);
        let Value::Relationship(rel) = &rels[0] else {
            panic!("virtual relationship, got {:?}", rels[0]);
        };
        assert_eq!(rel.rel_type, "TRANSFER_TO");
        // ADR-0187: the property map is exactly `{name: <relType>}`.
        assert_eq!(
            rel.properties.as_ref(),
            &BTreeMap::from([("name".into(), string("TRANSFER_TO"))])
        );
    }

    #[test]
    fn schema_visualization_signature_declares_nodes_relationships_outputs() {
        let signature =
            crate::procedure_registry::builtin_procedure_signature("db.schema.visualization")
                .expect("db.schema.visualization is registered");
        assert!(signature.executable);
        assert!(signature.listed);
        let outputs: Vec<(&str, &str)> = signature
            .returns
            .iter()
            .map(|ret| (ret.name, ret.type_str))
            .collect();
        assert_eq!(
            outputs,
            [
                ("nodes", "LIST<NODE>"),
                ("relationships", "LIST<RELATIONSHIP>")
            ]
        );
    }

    /// LABS-257 follow-up: the relationship half enumerates the per-type
    /// cartesian product of start and end labels, like the reference — a
    /// chain `A→B→C` via one rel type yields A→B, B→B, B→C, and A→C, not
    /// just the two pairs present in the data.
    #[test]
    fn schema_visualization_rel_types_are_the_cartesian_of_start_and_end_labels() {
        let graph = Graph::default();
        let a = graph
            .create_node(vec!["A".into()], map(&[("n", Value::Int(1))]))
            .unwrap();
        let b = graph
            .create_node(vec!["B".into()], map(&[("n", Value::Int(2))]))
            .unwrap();
        let c = graph
            .create_node(vec!["C".into()], map(&[("n", Value::Int(3))]))
            .unwrap();
        graph.create_rel(a, b, "NEXT".into(), map(&[])).unwrap();
        graph.create_rel(b, c, "NEXT".into(), map(&[])).unwrap();

        let rows = rows(&graph, "db.schema.visualization");
        let Value::List(rels) = &rows[0]["relationships"] else {
            panic!("relationships is a list");
        };
        let mut triples: Vec<(String, String)> = rels
            .iter()
            .map(|v| {
                let Value::Relationship(rel) = v else {
                    panic!("virtual relationship, got {v:?}");
                };
                // ids: A=-1, B=-2, C=-3 (labels sorted)
                let name = |id: i64| match id {
                    -1 => "A",
                    -2 => "B",
                    -3 => "C",
                    other => panic!("unexpected virtual id {other}"),
                };
                (name(rel.start).to_string(), name(rel.end).to_string())
            })
            .collect();
        triples.sort();
        assert_eq!(
            triples,
            vec![
                ("A".to_string(), "B".to_string()),
                ("A".to_string(), "C".to_string()),
                ("B".to_string(), "B".to_string()),
                ("B".to_string(), "C".to_string()),
            ],
            "the cartesian product of start labels × end labels"
        );
    }
}
