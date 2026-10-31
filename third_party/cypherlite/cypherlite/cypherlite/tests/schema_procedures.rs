//! Public-API integration tests for the inferred-schema procedures
//! `db.schema.nodeTypeProperties` and `db.schema.relTypeProperties`
//! (ADR-0112): scanning stored entities, grouping by full sorted label set /
//! relationship type, `valueType()` type names, mandatory = present on every
//! entity of the category, category order deterministic, and the
//! property-less null-row rule.

mod common;

use cypherlite::Value;

use common::{assert_unknown_output_fails, connection, string};

fn strings(values: &[&str]) -> Value {
    Value::List(values.iter().map(|v| string(v)).collect::<Vec<_>>().into())
}

/// Fixture covering every acceptance shape:
/// - two single-label node kinds: `:Person` (name) and `:Movie` (title);
/// - one multi-label node grouping under its full sorted label set;
/// - one relationship type `:ACTED_IN` with an optional `roles`;
/// - `:Person.age` present on some but not all nodes (optional);
/// - `:Movie.year` observed as both an integer and a string (mixed type).
fn fixture() -> cypherlite::Connection {
    let mut connection = connection();
    connection
        .query(
            "CREATE (a:Person {name: 'Alice', age: 36}), \
                    (b:Person {name: 'Bob'}), \
                    (c:Person:Employee {name: 'Carol', department: 'Eng'}), \
                    (m:Movie {title: 'Inception', year: 2010}), \
                    (n:Movie {title: 'Dune', year: '2021'}), \
                    (a)-[:ACTED_IN {roles: 'Dom Cobb'}]->(m), \
                    (b)-[:ACTED_IN]->(n)",
        )
        .unwrap();
    connection
}

#[test]
fn node_type_properties_reports_exact_rows_in_category_order() {
    let mut connection = fixture();
    let result = connection
        .query(
            "CALL db.schema.nodeTypeProperties() \
             YIELD nodeType, nodeLabels, propertyName, propertyTypes, mandatory \
             RETURN nodeType, nodeLabels, propertyName, propertyTypes, mandatory",
        )
        .unwrap();

    assert_eq!(
        result.header.columns,
        [
            "nodeType",
            "nodeLabels",
            "propertyName",
            "propertyTypes",
            "mandatory",
        ]
    );
    // Categories sorted by nodeType (`:Employee:Person` < `:Movie` < `:Person`),
    // properties sorted by propertyName within each category.
    assert_eq!(
        result.rows,
        vec![
            vec![
                string(":Employee:Person"),
                strings(&["Employee", "Person"]),
                string("department"),
                strings(&["STRING NOT NULL"]),
                Value::Bool(true),
            ],
            vec![
                string(":Employee:Person"),
                strings(&["Employee", "Person"]),
                string("name"),
                strings(&["STRING NOT NULL"]),
                Value::Bool(true),
            ],
            vec![
                string(":Movie"),
                strings(&["Movie"]),
                string("title"),
                strings(&["STRING NOT NULL"]),
                Value::Bool(true),
            ],
            vec![
                string(":Movie"),
                strings(&["Movie"]),
                string("year"),
                // Same key observed as INTEGER and STRING within the category.
                strings(&["INTEGER NOT NULL", "STRING NOT NULL"]),
                Value::Bool(true),
            ],
            vec![
                string(":Person"),
                strings(&["Person"]),
                string("age"),
                strings(&["INTEGER NOT NULL"]),
                // Present on Alice but not Bob: optional.
                Value::Bool(false),
            ],
            vec![
                string(":Person"),
                strings(&["Person"]),
                string("name"),
                strings(&["STRING NOT NULL"]),
                Value::Bool(true),
            ],
        ]
    );
}

#[test]
fn rel_type_properties_reports_exact_rows() {
    let mut connection = fixture();
    let result = connection
        .query(
            "CALL db.schema.relTypeProperties() \
             YIELD relType, propertyName, propertyTypes, mandatory \
             RETURN relType, propertyName, propertyTypes, mandatory",
        )
        .unwrap();

    assert_eq!(
        result.header.columns,
        ["relType", "propertyName", "propertyTypes", "mandatory"]
    );
    assert_eq!(
        result.rows,
        vec![vec![
            string("[:ACTED_IN]"),
            string("roles"),
            strings(&["STRING NOT NULL"]),
            // Present on the first ACTED_IN but not the second: optional.
            Value::Bool(false),
        ]]
    );
}

#[test]
fn category_without_properties_yields_one_null_row() {
    // ADR-0112: a stored category with no properties is still listed, as
    // exactly one row with a null property name and an empty type list.
    let mut connection = connection();
    connection.query("CREATE (:Empty)").unwrap();

    let result = connection
        .query(
            "CALL db.schema.nodeTypeProperties() \
             YIELD nodeType, propertyName, propertyTypes, mandatory \
             RETURN nodeType, propertyName, propertyTypes, mandatory",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![
            string(":Empty"),
            Value::Null,
            Value::List(Vec::new().into()),
            Value::Bool(false),
        ]]
    );

    connection
        .query("CREATE (a:Start {x: 1})-[:EMPTY_REL]->(b:Start {x: 2})")
        .unwrap();
    let result = connection
        .query(
            "CALL db.schema.relTypeProperties() \
             YIELD relType, propertyName, propertyTypes, mandatory \
             RETURN relType, propertyName, propertyTypes, mandatory",
        )
        .unwrap();
    assert_eq!(
        result.rows,
        vec![vec![
            string("[:EMPTY_REL]"),
            Value::Null,
            Value::List(Vec::new().into()),
            Value::Bool(false),
        ]]
    );
}

#[test]
fn empty_database_yields_zero_rows() {
    let mut connection = connection();
    for procedure in [
        "db.schema.nodeTypeProperties()",
        "db.schema.relTypeProperties()",
        "db.schema.visualization()",
    ] {
        let result = connection.query(&format!("CALL {procedure}")).unwrap();
        assert!(
            result.rows.is_empty(),
            "{procedure} on an empty database must return zero rows, got {:?}",
            result.rows
        );
    }
}

/// LABS-389 (CIP-277 compat smoke): the call shape with an options map is
/// accepted, not failed -- the map is ignored, the zero-arg output shape is
/// the documented form either way.
#[test]
fn schema_visualization_accepts_an_options_map() {
    let mut c = connection();
    c.execute("CREATE (:Person {name: 'Alice'})-[:KNOWS]->(:Person {name: 'Bob'})")
        .unwrap();
    let result = c
        .query(
            "CALL db.schema.visualization({nodeCount: 100}) YIELD nodes, relationships \
             RETURN nodes, relationships",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 1);
}

/// LABS-389 (the paren-less follow-up): a procedure whose parameters all
/// carry defaults is callable paren-less in-query -- the implicit-pass
/// guard keys on required args, not declared ones.
#[test]
fn schema_visualization_is_callable_parenless_in_query() {
    let mut c = connection();
    c.execute("CREATE (:Person {name: 'Alice'})-[:KNOWS]->(:Person {name: 'Bob'})")
        .unwrap();
    let result = c
        .query("CALL db.schema.visualization YIELD nodes RETURN count(nodes) AS n")
        .unwrap();
    assert_eq!(result.rows, vec![vec![Value::Int(1)]]);
}

/// Multi-label, multi-rel-type fixture for `db.schema.visualization`:
/// `:Person` and `:Movie` single-label nodes, a `:Person:Employee`
/// multi-label node, and two relationship types (`ACTED_IN`, `MANAGES`).
fn visualization_fixture() -> cypherlite::Connection {
    let mut connection = connection();
    connection
        .query(
            "CREATE (a:Person {name: 'Alice'})-[:ACTED_IN]->(m:Movie {title: 'Dune'}), \
                    (e:Person:Employee {name: 'Carol'})-[:MANAGES]->(a), \
                    (e)-[:ACTED_IN]->(m)",
        )
        .unwrap();
    connection
}

#[test]
fn visualization_reports_one_virtual_node_per_label_and_rel_per_triple() {
    let mut connection = visualization_fixture();
    let result = connection
        .query(
            "CALL db.schema.visualization() YIELD nodes, relationships RETURN nodes, relationships",
        )
        .unwrap();

    assert_eq!(result.header.columns, ["nodes", "relationships"]);
    assert_eq!(result.rows.len(), 1);
    let Value::List(nodes) = &result.rows[0][0] else {
        panic!("nodes is a list, got {:?}", result.rows[0][0]);
    };
    let Value::List(rels) = &result.rows[0][1] else {
        panic!("relationships is a list, got {:?}", result.rows[0][1]);
    };

    // One virtual node per stored label, sorted, with negative ids and
    // Neo4j's `{name, indexes, constraints}` property shape.
    let mut node_ids = std::collections::BTreeMap::new();
    assert_eq!(nodes.len(), 3);
    for (index, expected_label) in ["Employee", "Movie", "Person"].iter().enumerate() {
        let Value::Node(node) = &nodes[index] else {
            panic!("virtual node, got {:?}", nodes[index]);
        };
        assert_eq!(
            node.labels.as_slice(),
            [expected_label.to_string()].as_slice()
        );
        assert!(node.id < 0, "virtual node id, got {}", node.id);
        assert_eq!(
            node.properties["name"],
            Value::String((*expected_label).into())
        );
        assert_eq!(node.properties["indexes"], Value::List(Vec::new().into()));
        assert_eq!(
            node.properties["constraints"],
            Value::List(Vec::new().into())
        );
        node_ids.insert(expected_label.to_string(), node.id);
    }

    // One virtual relationship per distinct (startLabel, relType, endLabel)
    // triple, sorted; the multi-label `e` endpoint expands to both
    // `:Employee` and `:Person` triples.
    let expected: &[(&str, &str, &str)] = &[
        ("Employee", "ACTED_IN", "Movie"),
        ("Employee", "MANAGES", "Person"),
        ("Person", "ACTED_IN", "Movie"),
        ("Person", "MANAGES", "Person"),
    ];
    assert_eq!(rels.len(), expected.len());
    for (rel, (start_label, rel_type, end_label)) in rels.iter().zip(expected) {
        let Value::Relationship(rel) = rel else {
            panic!("virtual relationship, got {rel:?}");
        };
        assert_eq!(rel.rel_type, *rel_type);
        assert_eq!(rel.start, node_ids[*start_label]);
        assert_eq!(rel.end, node_ids[*end_label]);
        assert!(rel.id < 0, "virtual relationship id, got {}", rel.id);
        // ADR-0187: the property map is exactly `{name: <relType>}`.
        assert_eq!(
            rel.properties.as_ref(),
            &std::collections::BTreeMap::from([("name".into(), string(rel_type))])
        );
    }
}

#[test]
fn visualization_virtual_relationships_name_matches_type() {
    // ADR-0187: each virtual relationship's property map is exactly
    // `{name: <relType>}`, so reading `r.name` off the returned value
    // yields the same string as `type(r)`. Rows come back in the
    // relationships list order (sorted triples; the multi-label `e`
    // endpoint expands to both `:Employee` and `:Person`).
    let mut connection = visualization_fixture();
    let result = connection
        .query(
            "CALL db.schema.visualization() YIELD relationships \
             UNWIND relationships AS r \
             RETURN type(r) AS t, r.name AS name",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        vec![
            vec![string("ACTED_IN"), string("ACTED_IN")],
            vec![string("MANAGES"), string("MANAGES")],
            vec![string("ACTED_IN"), string("ACTED_IN")],
            vec![string("MANAGES"), string("MANAGES")],
        ]
    );
}

#[test]
fn visualization_virtual_relationships_resolve_start_and_end_nodes() {
    // LABS-150: the virtual entities are registered in the CIP-257 side
    // table, so `startNode()`/`endNode()` resolve them without a store
    // lookup.
    let mut connection = visualization_fixture();
    let result = connection
        .query(
            "CALL db.schema.visualization() YIELD relationships \
             UNWIND relationships AS r \
             RETURN type(r) AS t, startNode(r).name AS s, endNode(r).name AS e",
        )
        .unwrap();

    assert_eq!(
        result.rows,
        vec![
            vec![string("ACTED_IN"), string("Employee"), string("Movie")],
            vec![string("MANAGES"), string("Employee"), string("Person")],
            vec![string("ACTED_IN"), string("Person"), string("Movie")],
            vec![string("MANAGES"), string("Person"), string("Person")],
        ]
    );
}

/// Catalog fixture for `db.schema.visualization` (ADR-0166): a `:Client`
/// label carrying a uniqueness constraint on `id` (which also installs its
/// constraint-backed index) plus a standalone index on `name`.
fn visualization_catalog_fixture() -> cypherlite::Connection {
    let mut connection = connection();
    connection
        .query("CREATE (:Client {id: 1, name: 'Ada'})")
        .unwrap();
    connection
        .query("CREATE CONSTRAINT client_id_unique FOR (n:Client) REQUIRE n.id IS UNIQUE")
        .unwrap();
    connection
        .query("CREATE INDEX client_name_index FOR (n:Client) ON (n.name)")
        .unwrap();
    connection
}

#[test]
fn visualization_populates_indexes_and_constraints_from_catalog() {
    // LABS-172 / ADR-0166: the virtual label node's `indexes`/`constraints`
    // come from the catalog. The uniqueness constraint renders in Neo4j's
    // space style with no id/ownedIndex fields; its constraint-backing index
    // is excluded from `indexes` (Neo4j lists it only as ownedIndex on the
    // constraint string), leaving only the standalone "name" index.
    let mut connection = visualization_catalog_fixture();
    let result = connection
        .query("CALL db.schema.visualization() YIELD nodes RETURN nodes")
        .unwrap();

    assert_eq!(result.rows.len(), 1);
    let Value::List(nodes) = &result.rows[0][0] else {
        panic!("nodes is a list, got {:?}", result.rows[0][0]);
    };
    assert_eq!(nodes.len(), 1);
    let Value::Node(node) = &nodes[0] else {
        panic!("virtual node, got {:?}", nodes[0]);
    };
    assert_eq!(node.labels.as_slice(), ["Client".to_string()].as_slice());
    assert_eq!(node.properties["indexes"], strings(&["name"]));
    assert_eq!(
        node.properties["constraints"],
        strings(&[
            "Constraint( name='client_id_unique', type='NODE PROPERTY UNIQUENESS', schema=(:Client {id}) )"
        ])
    );
}

#[test]
fn yield_of_unknown_column_fails_validation() {
    // YIELD binds only columns the procedure actually declares. An explicit
    // YIELD of a name the procedure does not return is rejected at compile
    // time (`UnknownProcedureOutput`) instead of silently resolving to a
    // null value. The empty-database zero-rows case is asserted separately in
    // `empty_database_yields_zero_rows`. YIELD validation happens at
    // compile time, so the check needs no fixture data.
    for (call, unknown) in [
        ("CALL db.schema.nodeTypeProperties()", "notAColumn"),
        ("CALL db.schema.relTypeProperties()", "noSuchColumn"),
        ("CALL db.schema.visualization()", "notAColumn"),
    ] {
        assert_unknown_output_fails(&format!("{call} YIELD {unknown} RETURN {unknown}"), unknown);
    }
}
